# Audit — verified status

What is true of the code today, checked against the code and each
milestone's acceptance criteria. The plan is in `ROADMAP.md`. Each milestone
file under `milestones/` has the full evidence.

Status key:
- 🟢 met and verified;
- 🟡 met, with a named gap;
- 🔴 violated: a real defect (none open today);
- 🚫 not verified yet (needs hardware or CI, not a known code gap);
- — planned, not started.

*Last updated: Milestone 41, site isolation (2026-10-22).*

## Headline numbers

| Check | Value | How to reproduce |
|---|---|---|
| Tests | **1,209 passed, 0 failed** (1,195 before M41; the gap pass's tests had not been counted here) | `cargo test --workspace --release` |
| Fuzz suite | 25 targets; M41 added six `ipc_decode` seeds (`21`–`26`: storage requests and answers served by the browser, an unknown operation, a value claiming 2 GiB); since M40b `parse_document` also moves the tree into a `Document` and checks every interned name reads back (seed `14_invented_names.html`); the newest target is M40's `webidl_parse` (the binding declarations, malformed and deeply nested input), and `js_script/07_generated_bindings` calls prototype members on the wrong `this`, tears accessors off and forges node ids; earlier seeds cover the M38b renderer protocol (`ipc_decode`) and the M39 tree mutation API (`js_script/06_tree_mutations`); `layout_document` also checks M39b's pruned hit testing against visiting every box. Replay, structural stress and deterministic mutation all clean. | `cargo test --manifest-path fuzz/Cargo.toml --no-default-features` |
| AddressSanitizer | 0 reports: `js`, `engine` and display-list suites, 3 runs (the CI `sanitizer` job's command); M34 re-ran them with `gpu_backend` (the GPU tests on hardware included), 2 runs; M35 with `text` too; M36 with `browser_core`, `capability` and `storage`, 1 run; M37 with `network`, `filter` and `privacy`, 1 run; M38a re-ran the CI command on Boa 0.22, 3 runs; M38b, 3 runs; M39 with `dom` added, 3 runs; M39b with `layout`, `rendering`, `text`, `style` and `dom`, 3 runs; M39c the same, 3 runs; M40 with `script` (now in the CI job) and `dom`, 4 runs; M40b with `names`, `dom`, `html_parser`, `css_parser` and `style`, 3 runs; M41 with `storage` and `capability` (site isolation, browser-served storage, a fake compromised renderer), 3 runs. | `.github/workflows/ci.yml`, job `sanitizer` (nightly) |
| Clippy | 28 warnings (a ratchet; it may not grow — 43 until M40 removed the hand-written bindings; M40b and M41 added none) | `scripts/clippy-ratchet.sh` |
| `unsafe` | None. `forbid(unsafe_code)` in every crate, and the exceptions list is empty. | `scripts/check-forbid-unsafe.sh` |
| Guardrails | All pass | `check-dependency-direction.sh`, `check-host-boundary.sh`, `check-panic-strategy.sh`, `check-gc-handles.sh`, `check-core-capabilities.sh`, `check-capabilities.sh` (M36), `check-dom-encapsulation.sh` (M39), `check-advisory-exceptions.sh`, `check-psl-freshness.sh` |
| `cargo audit` | 2 documented exceptions, both unmaintained-crate notices: `paste` (build-time, via wgpu's `metal`) and `ttf-parser` (the font parser, ADR 009). The two Boa 0.17 advisories are gone with Boa 0.22 (M38a). | `deny.toml`, `scripts/check-advisory-exceptions.sh` |
| Renderer sandbox | Active on this machine (Linux 7.2): Landlock closes the filesystem; seccomp refuses sockets, exec and ptrace. Seen in `/proc` of a running renderer: seccomp filter, `NoNewPrivs`, no sockets, empty environment. | `browser-renderer --sandbox-self-test`; `crates/renderer/tests/real_processes.rs` |
| Real-site corpus | Last replayed at M40b (its recordings are local-only and were not on the machine M41 was built on; it replays in thread mode, which M41 did not change): 56 of 56 pages render, with 0 crashes and 0 timeouts. All are deterministic and match their goldens (re-blessed in M34 after a per-pixel comparison: 43 identical, 13 differing in 1–595 pixels, each explained). | `cargo run -p conformance --release -- run` (the corpus is local-only, not in the repository) |

## Principles

The code was audited against each principle on 2026-09-24. The P-numbers refer
to `ROADMAP.md` §3, "Where the principles are violated today", which gives
each violation's detail and the work item that fixes it.

| Principle | Status | Evidence / gap |
|---|---|---|
| Memory-safe | 🟢 | No `unsafe` in our code; every crate forbids it (in source or in `[lints.rust]`), and CI enforces it. About 25 production panic sites, all justified invariants. Locks are poison-tolerant. A heap use-after-free in Boa's thread teardown, reached through our untraced captures (P12), was found under AddressSanitizer and fixed (ADR 015); since M40 no untraced field holds a GC handle — the runtime keeps every handle in traced tables and the host holds ids (ADR 023 §5) — and a guardrail and a CI sanitizer job keep it so. |
| GPU-first | 🟢 | Everything on screen is drawn by the GPU, through `wgpu` and a display list with clip, transform and group markers (ADR 017); the CPU rasterizer is only for `conformance` PNGs and tests. Clips and transforms per instance on the GPU, opacity groups as offscreen layers, a kept back buffer redrawn by damage, image atlas pages with LRU eviction, batching across non-overlapping items. Transform animations and container scrolls are compositor updates; after an incremental layout the page's display list is updated in part (M39b), and patched in place when the changed element keeps its size (M39c, ADR 022). The GPU path is checked against the CPU rasterizer on hardware (`gpu_matches_raster.rs`). |
| Thread-safe | 🟢 | One OS thread per page (in its own process by default, ADR 016) owns its non-thread-safe state (`Rc`/`RefCell` only in `engine` and `js`). There's a GPU thread and a thread-safe mailbox, checked at compile time. Global state is limited to atomics, read-only tables built once, and the bounded fetch pool's queue (one lock). `rayon` runs style (per-worker result shards), intrinsic sizing (sharded) and image decoding, including a page's network images — which decode off the page thread (M34). |
| Capability-based | 🟢 | Script reaches the network and storage only through capability handles the profile grants each page (ADR 010, M36): no grant, no `fetch`/XHR/`localStorage` — a capability error, checked in thread and process mode. Grants are scoped (storage to the document's own storage key) and revocable (`PageCommand::Revoke`, in-flight requests included). APIs not built have capability types that cannot be constructed. Minting only in Browser Core and the renderer, and no global mutable state in the bindings (`js` and, since M40, `script`) — `check-capabilities.sh`. Only `network` opens connections, in the browser process; file access (`FileAccess`, denied to pages), cookie and profile files, and host-granted fonts are the filesystem's only users; no core crate reads the environment or starts processes (`check-core-capabilities.sh`). Since M41 the browser process enforces the grants too (ADR 025): without the network grant it makes only the document loader's requests, whatever the renderer labels them, and of a cross-origin response gives a renderer only what the request's stated purpose can use; `localStorage` is served by it, through the page's own grant. |
| Zero-trust | 🟡 | Every page runs in its own sandboxed renderer process (ADR 016), which holds documents of one site: a navigation that lands on another site moves the page to another renderer (ADR 025, M41); the browser decodes what it sends with a bounded, fuzzed codec and makes its network requests and storage operations only for origins of that site it actually loaded. Parsers are bounded and fuzzed, page commands are validated, and redirects to non-web schemes are refused. GPU resources are bounded: a capped glyph atlas that compacts, every texture checked against device limits, an image-texture budget kept by LRU eviction (M34), and a GPU error handler instead of a panic (P1–P3, fixed and verified on hardware; `milestones/principles-audit-fixes.md`). **Gap:** no CI or libFuzzer run has been observed (a GitHub remote exists; its Actions history was not checked, and there is no `g++` locally). |
| Privacy-first | 🟢 | Every row of the §6 boundary has a named policy and a test (table in `milestones/milestone-37-privacy-layer.md`): routes that fail closed with a fixed precedence (ADR 011); DNS over HTTPS with no default vendor (ADR 012); third-party cookies blocked and storage keyed by (top-level site, origin) (ADR 019); a filter engine on every hop in the browser process; a fingerprint policy for `Date`, rAF and fonts, through Boa's host hooks (P5); frozen headers; no `navigator`, `screen` or `performance`; no phone-home. |
| Vendor-independent | 🟢 | No Blink, Gecko or WebKit; `cargo deny` bans encode it, and no service is hard-coded: no default DoH resolver, no bundled third-party filter list. TLS trusts the bundled Mozilla roots plus any CA the user adds (P8, ADR 018). |
| Secondary: power budget, minimal-dependency | 🟡 | A runaway page in a renderer process is ended within 10 s (ADR 013, M38a); pages on threads are bounded only by Boa's loop and recursion limits: the runtime interface has an interrupt (M40, ADR 023 §6), but Boa 0.22 offers none. Pages are woken by finished work, never polled; hidden pages do no animation work (a hidden renderer measured at 0 CPU ticks in 2 s); an animating page that can't keep up lowers its frame rate and input is not delayed (ADR 020, M38b). Off-screen `background-color` transitions are skipped (gap pass); other off-screen animations are not; a covered or minimised window hides its active tab (gap pass). `engine`'s exact `rayon` pin is relaxed (P11), but `style` and `images` still pin `rayon = "=1.10.0"`. |

## Per-milestone status

| # | Milestone | Status | Verified / open |
|---|---|---|---|
| 1 | Vertical slice | 🟢 | Confirmed on real hardware (AMD Radeon 780M, Wayland). |
| 2 | Text rendering | 🟢 | Confirmed on hardware. Shaping, fallback and bidi since M35. |
| 3 | HTML parsing & fuzzing | 🟢 | Fuzzed in `cargo test`. libFuzzer runs 🚫 (no `g++`). |
| 4 | CSS selectors & cascade | 🟢 | Extended by Phase A and M33 (UA origin, `:root`, nesting). `:hover`, `:active`, `:focus` and `:focus-within` match since the gap pass (`style::Interaction`; a change restyles only the elements it changed, counted). |
| 5 | Inline layout | 🟢 | UAX #14 line breaking since M35. |
| 6 | Box model | 🟢 | Per-side borders since the gap pass: every side's width, style and colour (`border-top` …, longhands, one-to-four-value forms), in layout and paint; 39 goldens re-blessed with each difference classified. |
| 7 | Images | 🟢 | Decoding is parallel and tested. GPU image drawing checked against the CPU rasterizer on hardware (M34, `gpu_matches_raster.rs`). |
| 8 | Networking | 🟢 | Real pages recorded through the real network stack (M32). |
| 9 | Cookies | 🟢 | Extended by the hardening pass and M28. |
| 10 | Flexbox | 🟢 | Rewritten on Flexbox §9.7, with column grow and shrink (M33). |
| 11 | JavaScript (Boa) | 🟢 | Boa 0.22 since M38a; one runtime behind `script::ScriptRuntime` since M40 (ADR 023). Boa has no interrupt; in process mode the watchdog bounds a runaway page (ADR 013). |
| 12 | JS storage & `fetch` | 🟢 | Non-blocking `fetch`, and storage partitioned by origin. |
| 13 | DOM mutation re-render | 🟢 | Incremental since M31; the full mutation API since M39. |
| 14 | Animations & rAF | 🟢 | Declarative animation done in M22. |
| 15 | Input | 🟢 | Click and Tab confirmed on hardware by a human. |
| 16 | Forms | 🟢 | Enter submits a form (implicit submission, HTML §4.10.21.2: the default button is clicked, or a lone text field submits) and activates a focused button — closed in the gap pass (2026-10-21). |
| 17 | Integration | 🟢 | `integration_tests` crate. Real sites through the M32 corpus. |
| 18 | Detached DOM nodes | 🟢 | Detached subtrees are `dom` fragments since M39, and a removed node comes back as one (ADR 021); since M40 a node has one wrapper, attached or not, so the "stale handle" gap is gone (ADR 023). |
| 19 | Caret & specialized inputs | 🟢 | `range`, `date`, `color` and `file` inputs are in the backlog. |
| 20 | Event model & validation | 🟢 | |
| 21 | Page navigation | 🟢 | History and POST are in the backlog. |
| 22 | Animation depth | 🟢 | `rotate()`/`skew()` draw since M34; transform animations of promoted elements run on the compositor. `@keyframes` keeps every stop and each property runs on its own keyframes, with `animation-timing-function` (per stop too) since the gap pass. |
| 23 | Rich input & editing | 🟡 | Selection, drag-and-drop, context menus and the file picker are in the backlog. |
| 24 | Accessibility tree | 🟡 | The OS screen-reader bridge is in the backlog. |
| — | Hardening pass | 🟢 | Items not fixed there were taken up by M34–M38 (as 38a and 38b). |
| 25 | Guardrails & CI | 🟡 | CI workflows and libFuzzer jobs are written; no run observed: the repository has a GitHub remote (`origin`), but its Actions history was not checked 🚫. |
| 26 | Shared network context | 🟢 | |
| 27 | Observability | 🟢 | Live-window `--trace` and the `gpu_render` span exercised on hardware in M34 (per-frame spans, including `compositor_update`). |
| 28 | Origin & site model | 🟢 | |
| 29 | Engine facade | 🟢 | Real hardware found double sRGB encoding and an unthrottled loop; both are fixed. |
| 30 | Page threads | 🟢 | 60 fps on hardware, and frame build no slower (0.94 → 0.87 ms median). |
| 31 | Incremental pipeline | 🟢 | Oracles are merge gates. Scroll is 7.6× faster. The first frame is about 17% slower (measured). |
| 32 | Conformance harness | 🟡 | The corpus predates web fonts (a re-record will add them). The `fontdue` hang is resolved (M35). |
| 33 | Layout completeness | 🟡 | Per-side borders done (gap pass); list markers are in the backlog. The paint-only cost it added was re-measured in M34: gone for scroll/DOM/caret frames. |
| — | Principles-audit fixes | 🟢 | Hostile glyphs and images bounded. Verified on hardware: 60 fps, no panic, one report per second. |
| — | Performance-audit fixes | 🟢 | Linear selector matching (was exponential); scroll/caret frames O(viewport); intrinsic sizing incremental; bounded fetch pool; progressive images; the Boa use-after-free (P12). Checked on hardware: 218 frames in 8 s, no GPU error. |
| — | Process isolation | 🟢 | Renderer process per page, sandboxed, behind the unchanged API; network via the browser with an origin lock; scenes identical to thread mode (tested). Hardware: renderer seen sandboxed, frames drawn. ~~Site-per-process, shared storage~~ (M41); non-Linux sandboxes → backlog. |
| 34 | Compositing & GPU depth | 🟢 | Clips, transforms (`rotate`/`skew`), rounded clips, opacity groups, compositor transform animation and container scrolls, back buffer with damage, image atlas + LRU eviction, batching (211 → 3 draw calls on an image-heavy page), glyph-atlas compaction, off-thread decoding. GPU path checked against the CPU on hardware; 58 fps, 345/348 frames on the compositor path. ~~**Gaps:** A5 (relative layout positions) and partial display-list rebuilds → M39b.~~ Both done in M39b (ADR 022). |
| 35 | Text subsystem | 🟡 | Shaping, fallback, the text properties, bidi, UAX #14, web fonts (ADR 009); verified through real sandboxed renderers with system fonts. **Gaps:** the script snapshots' human sign-off 🚫; web fonts absent from the recorded corpus; first frames +18% on a 10k-row page. |
| 36 | Capabilities & Browser Core | 🟡 | `capability`, `storage` and `browser_core` crates; grants per page, scoped and revocable; settings and permissions persisted and validated; tabs, zoom, `--profile` (ADR 010). Hardware: storage persisted and read back across restarts (thread mode). **Gaps:** ~~process-mode storage and network enforcement~~ (M41); ~~no re-grant to a live page~~ (gap pass: `PageCommand::Grant`); shortcuts not pressed by a person. |
| 37 | Privacy layer | 🟡 | Routes (proxy, system VPN, precedence, fail closed), DoH, filter engine, third-party cookie and storage isolation, fingerprint policy (`Date`, rAF, fonts), extra CA roots (ADR 011, 012, 018, 019). **Gaps:** WireGuard evaluated only; ~~no live TLS test with a private CA~~ (gap pass: a live `rustls` server); `socks` 0.3 is old. |
| 38a | Script time bound & Boa upgrade | 🟡 | A renderer that owes a tick for 10 s with no progress is killed and its page reported crashed (ADR 013), tested in-process and with a real sandboxed renderer; Boa 0.17 → 0.22 (no default features), both advisory exceptions removed. **Gaps:** thread mode not bounded; no script interrupt (M40 added the interface; Boa offers none); not exercised in the live app. |
| 38b | Frame and power budget | 🟡 | Wake on completion instead of 8 ms polling; `SetVisible` (Session hides background tabs); a frame rate that steps 60 → 4 fps under load and back, input never degraded (ADR 020). Measured: a hidden renderer 0 ticks vs 44 visible in 2 s; a 30 ms/frame page settles at 15 fps. **Gaps:** ~~off-screen transitions not skipped~~ (gap pass: `background-color` ones, 0 frames vs 36); ~~window occlusion not a visibility source~~ (gap pass; not seen on a desktop); not exercised in the live app. |
| 39 | Arena DOM | 🟢 | Readers through `ElementRef`; an arena of linked slots; the full mutation API in `dom` and `js`, O(1) link updates asserted by a counter; one validity/editability rule; `check-dom-encapsulation.sh` (ADR 021). Oracles extended; goldens unchanged. ~~**Gaps:**~~ interned names done in M40b (ADR 024); A5 and partial display lists done in M39b. (The "DOM-change" slowdown it recorded was a click that changed nothing — a benchmark bug M39b found; its cost was the full-tree hit test, pruned in M39b.) |
| 39b | Layout coordinates & partial display lists | 🟡 | Box frames: a reused subtree moves in O(1) (counted), bookkeeping kept across frames; subtree bounds — a click on 10k rows examines 5 boxes (counted); partial display-list updates — ≤ 12 boxes built for a one-row change (counted); node-id hashing; a sticky box in a moved container was drawn in the wrong place (fixed; one golden re-blessed, explained per pixel). Oracles: layout (document space, bounds), page-list update vs full build, pruned vs full hit testing. A real one-element change on 10k rows 25 → 7 ms; first frames of the 1k/10k pages 2–8% faster. **Gaps:** fresh layouts still build per frame; the demo's first frame 0.05 ms slower and 1k/10k scroll frames 2 µs slower (the last criterion not met). (The O(page) change work it left is gone for size-keeping changes in M39c.) |
| 39c | Changes that keep their size, patched in place | 🟢 | A change whose element keeps its size is laid out alone and spliced into the previous layout and display list; intrinsic sizes measured at the change (ADR 022 §8–10). Counters: 1 element laid out, nothing moved, ≤ 5 boxes built on 10k rows. Oracles: patched layouts (over 600 patched frames), patched page lists (over 300). A one-element change on 10k rows 7 ms → 0.33 ms. |
| 40 | Script runtime interface & generated bindings | 🟡 | `script::ScriptRuntime` with Boa (`js`) and a stub runtime behind it; the host side (DOM operations, events, forms, editing, `fetch`/XHR, storage) runtime-independent in `script`; bindings generated from WebIDL (13 interfaces, 64 members) through a checked `dispatch`; one wrapper per node, held weakly, its node in an internal slot (no more forgeable `__node_id`); GC handles traced, `gc-handle-captures.txt` empty (ADR 023). 17 binding cases pass on both runtimes; 10,000 `getElementById` calls make one wrapper (counted). Script-heavy frames level or faster; goldens unchanged. **Gaps:** ~~interned names → M40b~~ (done there); Boa cannot be interrupted; ~~expandos on collected wrappers~~ (kept for nodes in the document, gap pass; detached nodes remain); ~~`window`/`self`~~ (gap pass: the global, an `EventTarget`; goldens unchanged). |
| 41 | Site isolation & browser-process storage | 🟡 | A renderer holds one site's documents: the host delivers a navigation response only of the renderer's site, else the page moves to a renderer that has held nothing, handed the response (fetched once), behind the unchanged API — links, typed URLs, redirects; the old document shown until the new one commits; a failure moves nothing. `localStorage` served by the browser from the profile's store through the page's grant (pages of one origin share it in process mode). The browser enforces the network grant and withholds what a mislabelled request cannot use (ADR 025). Tested with in-process renderers and real sandboxed ones (another pid, the old one gone); a fake compromised renderer cannot get a script request made without the grant. A document is named by the URL it landed on (closing M21's simplification). **Gaps:** no sandbox on macOS/Windows (needs FFI and those machines); not driven across sites in the live app 🚫. |
| 40b | Interned names | 🟢 | A `names` crate: well-known atoms equal everywhere (233 names, ranked by the corpus), a `Name` that is an atom or its own text, a per-document `AtomTable` (ADR 024, implementing ADR 021 §5). Born interned in the tokenizer, `createElement` and the CSS parser; interned in place when a fragment enters a document, and by `setAttribute`. Readers and selectors compare atoms; the rule index keys well-known types by atom. A million invented names grow only their document's table (counted). Measured against M40: corpus parse 7–10% faster, 10k first frames 7–8%, DOM changes 10%, a selector-heavy restyle 3–6%; goldens unchanged. Load average 3–7 while measuring (as M40's). |

## Open gaps, and where they go

| Gap | Goes to |
|---|---|
| `letter-spacing`, `word-spacing`, `overflow-wrap`, `word-break`, `text-overflow`, `font-variant`, `unicode-range`, dictionary line breaking | Backlog (from M35) |
| WireGuard route (needs a user-space TCP/IP stack), macOS/Windows trust stores, HSTS and pinning, cosmetic filter rules | Backlog (from M37) |
| Skipping off-screen `@keyframes` animations and transitions that can move a box | Backlog (from M38b) |
| `about:engine`, deterministic replay, thermal hooks | Backlog (split out of M38) |
| A script interrupt in the runtime (Boa 0.22 has none; the interface has one since M40) | Backlog: a runtime that offers one |
| A change that alters its element's size: its parent visits all its children, the page list is updated in O(items); patching is ruled out conservatively by out-of-flow boxes and opacity/transform/scroll ancestors; a page with a sticky box builds its list every frame | Backlog (from M39b, M39c) |
| Attribute values (`class`, `id`) interned, so class selectors compare atoms | Backlog (from M40b) |
| Expandos on a collected wrapper of a *detached* node (nodes in the document keep their wrappers since the gap pass); the IDL deviations marked in `crates/script/webidl/` | Backlog (from M40) |
| Sandboxes beyond Linux (macOS `sandbox_init`/App Sandbox, Windows AppContainer): FFI with an `unsafe` exception, and a machine of each platform to build and self-test on | Backlog (from M41) |
| List markers, `:checked`/`:*-of-type`/`:has()`, `@supports`, gradients, `cursor`, window events (`load`, `resize`), `location` | Backlog, ranked by the corpus (`ROADMAP.md` §4) |
| Video (`<video>`, codecs, A/V sync): never scoped; `<video>` is only sized as a replaced element (`layout::intrinsic`, `style::cascade`). Flagged in the 2026-09-02 audit | Needs its own scoping pass (`ROADMAP.md`, "Video") |
| Every other item any milestone (1–33, Phases A–F, the hardening pass) deferred: events, navigation, frames, network, forms, a11y, animation, performance | Backlog (`ROADMAP.md` §4), each tagged with the milestone that deferred it |
| Hardware checks: human input on the threaded window | 🚫 Needs a person at the desktop (GPU drawing, paint order and the live `--trace` window were exercised in M34) |
| A first CI run, libFuzzer runs | 🚫 A GitHub remote exists now (`origin`, holding the initial commit); whether its workflows ran is unchecked (no `gh` here). libFuzzer locally needs `g++` |

The engine is a well-tested core with enumerated, bounded gaps. It is not a
finished browser.

---

## Appendix: the 17-milestone audit (2026-09-02)

Kept for history. It was the project's first audit, written when it had 17
milestones and 297 tests. Everything above supersedes it: every 🟡 and 🚫 row
in its table is 🟢 above, and most of what it lists as out of scope has since
been built (column flex, `transition`/`@keyframes`, navigation, detached nodes,
accessibility, the privacy layer, capabilities, real-site testing). Video is
the exception; it is now in the open gaps above.

### Full 17-Milestone Audit

Checked against each milestone's own acceptance criteria (see
`milestones/milestone-NN-*.md` for the full detail behind every line
here — this document summarizes, it doesn't replace them). Status
legend: 🟢 fully met and verified · 🟡 met with a real, named gap ·
🔴 not implemented · 🚫 not verifiable in this sandbox (needs real
hardware/internet, not a code gap).

**297 tests pass, zero failures, across 11 platform-agnostic crates**
(`dom`, `html_parser`, `css_parser`, `style`, `layout`, `text`,
`images`, `network`, `js`, `rendering`, `integration_tests`).
Dependency-direction invariant holds across all of them
(`scripts/check-dependency-direction.sh`).

#### Per-milestone status

| # | Milestone | Status | Notes |
|---|---|---|---|
| 1 | Vertical Slice | 🚫 | Confirmed on real hardware in an earlier session (window opens, 3 boxes render, resize works). |
| 2 | Text Rendering | 🚫 | Platform-agnostic half (font loading, glyph rasterization, atlas) fully tested. GPU half written, unconfirmed since. |
| 3 | Real HTML Parsing & Fuzzing | 🟢 | Real tokenizer + error recovery, fully tested. Fuzz harness written, not run on real hardware (needs `cargo-fuzz`, a real machine). |
| 4 | CSS Selectors & Cascade | 🟢 | Full 4-pass cascade, specificity, inheritance — fully tested. |
| 5 | Inline Layout & Line Breaking | 🟢 | Real inline formatting context, word wrapping — fully tested. |
| 6 | Box Model Completeness | 🟢 | Margin/border/padding/content as distinct layers — fully tested. A real pre-existing bug (margin behaving like padding) was found and fixed here. |
| 7 | Images | 🟡 | Decode/cache/layout fully tested against real fixture images; decoding now parallel across cores (`preload_parallel`). GPU rendering unconfirmed. Video: not implemented, never scoped (see below). |
| 8 | Networking | 🟡 | `network` crate unusually strongly verified (real local HTTP server, real redirects, real background-thread concurrency). `app --url` mode was found dropped by a later rewrite and restored, now with concurrent stylesheet/image fetching. Real-website fetch unconfirmed (🚫, no internet access here). |
| 9 | Persistent Storage: Cookies | 🟢 | Full cookie jar, real two-request round-trip verified against a real local server. |
| 10 | Flexbox Layout | 🟡 | Row direction fully implemented (grow/shrink, justify, align). Column direction stacks/aligns but doesn't apply grow/shrink — documented scope reduction. |
| 11 | JavaScript Engine Integration | 🟢 | Boa embedded, DOM bindings, real bug (`<script>` text rendering as page content) caught on real hardware and fixed. |
| 12 | JS-Facing Storage & Fetch | 🟢 | `localStorage`/`sessionStorage`, and `fetch()` — now genuinely non-blocking (see concurrency work below), a real bug (synchronous throw instead of Promise rejection) caught by tests and fixed. |
| 13 | DOM Mutation & Reactive Re-render | 🟡 | Dirty-flag + `shell_desktop` tick mechanism implemented and tested at the logic level. GPU-visible confirmation unconfirmed. |
| 14 | CSS Animations, Transitions & rAF | 🟡 | `transform` + `requestAnimationFrame` fully implemented and tested. CSS `transition`/`@keyframes` interpolation explicitly NOT implemented — a deliberate, documented trade, not an oversight. |
| 15 | User Input & Interactivity | 🟡 | Hit-testing, click/keydown/scroll dispatch, bubbling, focus — all implemented and tested. Real-window input forwarding (`shell_desktop`) written, unconfirmed on real hardware. |
| 16 | Form Elements | 🟡 | Text editing (type/backspace/arrows/insert-at-cursor), tab order, and JS-observable form submission all implemented and tested. Named gaps: no visual caret indicator, no click-to-position, no classic non-JS form navigation, no Enter-to-submit. |
| 17 | Integration | 🟡 | Real-website testing 🚫 (no internet access). Subsystem-composition testing done a different way — a dedicated `integration_tests` crate proves the full pipeline (parse → cascade → layout → JS mutation → click dispatch → re-layout → paint) works together on combined documents, not just each piece in isolation. |

#### Concurrency & parallelism (explicitly requested as "the core" —
tracked separately from the numbered milestones since it wasn't one)

- **Real multi-core parallelism**: `style::resolve_style_tree_parallel`
  (via `rayon`) — every element's CSS cascade computed once, in
  parallel across CPU cores. Verified correct against a trusted
  sequential reference implementation on a 3,125-node synthetic tree.
  `layout` consumes this precomputed cache instead of resolving styles
  on demand, which also eliminated a previously-documented
  double-resolve inefficiency.
- **Real background-thread concurrency**: `network::DirectRoute::fetch_async`
  dispatches to a genuine OS thread; `js`'s `fetch()` now creates a
  real, initially-pending `Promise` settled later via
  `poll_pending_fetches`. Proven — not just asserted — with a
  deliberately-slow mock server and a timed test confirming the
  calling thread genuinely wasn't blocked.
- **What's still single-threaded, and why**: layout POSITIONING itself
  (block flow has genuine sequential dependencies — a structural
  reason, not a shortcut); paint/rasterization/GPU submission; `app`'s
  one-shot `--url` startup fetch (a natural next increment using the
  same `fetch_async` mechanism, not done yet).

#### What was implemented beyond the original 17 milestones' explicit scope

- Parallel style resolution (above) — not originally scoped as its own
  milestone.
- Background-thread `fetch()` (above) — same.
- `dom::element_ptr_key`/`find_by_ptr_key` — a safe identity-key
  mechanism used by BOTH the parallel style cache and hit-testing/event
  dispatch; genuinely reusable infrastructure, not single-purpose.
- `integration_tests` crate — not part of any milestone's explicit
  scope; created specifically to make Milestone 17's actual purpose
  (prove composition, not just isolation) verifiable in a sandbox with
  no display server and no internet access.

#### What remains genuinely out of scope — and an honest statement about why

**Video, first — because it's a different KIND of gap from everything
below.** Checked directly against the source: zero references to
`<video>`, video codecs, or media playback anywhere in this codebase,
and zero mentions across all 17 milestone documents' own scope
sections. Every other item in this section was explicitly deferred,
with a stated reason, at some milestone. Video wasn't deferred — it
was never scoped into the plan at all. Flagged here plainly rather
than left to be discovered by omission. See `ROADMAP.md`'s Phase 3
section for the fuller note on why it's genuinely its own future
milestone-scoping pass, not a quick addition to Milestone 7.

Every "out of scope" item explicitly deferred by each milestone
document remains deferred (see each milestone's own "Out of scope"
section and `ROADMAP.md`'s Phase 2/3 lists for the full itemized
lists). The largest, most consequential ones, named directly:

- **WebGL/WebGPU exposed to web content** — this engine USES a modern
  GPU abstraction (`wgpu`) for its OWN rendering; it does not expose
  `<canvas>`/`getContext('webgl')`/`navigator.gpu` to JavaScript. Real,
  separate, substantial work (a canvas 2D/WebGL/WebGPU API surface).
- **CSS Grid, `border-radius`, box-shadow, modern selectors, `@media`**
  — real layout/rendering features, genuinely absent.
- **Declarative CSS animation** (`transition`/`@keyframes`) — JS-driven
  `requestAnimationFrame` animation is a real, working substitute for
  the same visual outcome, but the CSS syntax itself isn't implemented.
- **Specialized form inputs** (checkbox/radio/range/date/color/file),
  client-side validation, `contenteditable`, `<select>` interactivity.
- **Page navigation** — this is a single-document engine; there is no
  "load a new page, replacing the current one" concept at all. This
  is the root cause of Milestone 16's classic-form-submission gap and
  would also be required for any real multi-page browsing experience.
- **The full DOM/JS API surface** — event capture phase,
  `preventDefault`/`stopPropagation`, `document.createElement` (a real
  architectural gap named in Milestone 13's audit — this crate's
  "re-find by id" object model has no way to represent a detached,
  not-yet-attached element), `XMLHttpRequest`, most of the DOM beyond
  the explicit allowlists each JS milestone scoped to.
- **Accessibility tree**, **extensions**, **multi-tab/browser chrome**,
  **non-Linux platform shells**, **the privacy/filter engine** (ad
  blocking, fingerprint resistance, `NetworkRoute` beyond
  `DirectRoute`) — all explicitly deferred from the very first
  architecture discussion, unchanged.
- **A from-scratch JS engine** — Boa remains the JS engine; nothing
  encountered in this session constituted the kind of real, concrete
  evidence (from real-site testing) the original design conversation
  said should trigger evaluating a replacement.

**On "implement everything left out of scope": this is not achievable
in any single engineering session, by any team.** The items above are,
collectively, the remaining work of an actual production web browser —
historically a multi-year, many-hundred-engineer undertaking (Chromium,
Firefox, Safari's WebKit). What this session added was targeted,
real, tested progress on the highest-value, best-scoped items
consistent with everything built before it — not a claim that the
engine is now feature-complete. The honest state of the project is
accurately reflected in the per-milestone table above and in each
milestone document's own "Implementation notes": a real, substantially
capable, well-tested engine core with clearly enumerated, bounded gaps
— not a finished browser.
