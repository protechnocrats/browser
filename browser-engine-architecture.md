# Browser Engine — Architecture Reference

An independent, from-scratch web engine. No Blink, Gecko, or WebKit dependency.
GPU-first, memory-safe, thread-safe, privacy-first, user-sovereign.

> No single vendor — not Apple, Google, Microsoft, a GPU vendor, a VPN
> provider, or us — should be architecturally necessary for this browser
> to exist or function.

---

## 1. Founding principles

Seven, kept visible everywhere — these constrain every design decision
below. Kept short deliberately, so they stay something engineers
actually reread rather than a manifesto that gets skimmed once:

> **Memory-safe. GPU-first. Thread-safe. Capability-based. Zero-trust.
> Privacy-first. Vendor-independent.**

- **Memory-safe** — no use-after-free, no buffer overflows, by construction.
- **GPU-first** — rendering pipeline assumes the GPU from day one; no
  "CPU renderer we accelerate later."
- **Thread-safe** — parallel by default where the problem allows it
  (parsing, style, layout); compile-time-checked, not
  convention-checked; concurrency designed in before DOM/layout
  structures, not bolted on after a single-threaded prototype.
- **Capability-based** — a process, origin, extension, or subsystem
  gets exactly the scoped, revocable authority it needs — never
  ambient authority. *(As built, ADR 010: authority is a handle that
  holds what it grants, minted by the profile, narrowed per document,
  revoked through a shared token.)*
- **Zero-trust** — network input, web content, the renderer, an
  extension, and the GPU process are all assumed potentially hostile
  or compromised; every boundary validates rather than trusts.
- **Privacy-first** — privacy is an architectural property (see §6),
  not a settings page or a bolt-on feature. Stated explicitly and kept
  as its own principle rather than folded into capability-based/
  zero-trust — those two constrain *security*; this one constrains
  what the browser is *allowed to expose or infer* about a user even
  from a fully "secure" API.
- **Vendor-independent** — no single company should be architecturally
  necessary: no Blink/Gecko/WebKit, no mandatory cloud, no mandatory
  VPN, no mandatory search engine, no mandatory telemetry, no
  platform-specific engine architecture. Open source, reproducible
  builds, exportable user data, no forced defaults. Includes a
  standing decision about ourselves, not just other vendors: **we do
  not operate VPN infrastructure** — see §6.1. Changing that is a
  deliberate, explicit decision to revisit this principle, not
  something a future feature quietly drifts into.

Secondary, real but reviewed at milestones rather than repeated as
mantra: standards-driven, accessible, energy-conscious, observable,
minimal-dependency, deterministic where practical.

---

## 2. Language decision: Rust (core), platform-native at the edges

**Rust for the engine core.** Decided and re-confirmed after direct
comparison against Swift, C#, F#, Haskell, Go, and Kotlin/Native.

The deciding axis is not "is it memory-safe" (several of those are) —
it's **predictable performance under adversarial, heavily concurrent
input while hitting a frame budget**. That rules out every
tracing-GC language for the *core*:

| Language | Why not for the engine core |
|---|---|
| Swift | No pure-language cross-platform GPU abstraction (no `wgpu` equivalent); Apple frameworks (CoreText/Metal) don't travel to Linux/Windows; ARC has real atomic-retain/release cost under heavy concurrency; ecosystem outside Apple platforms is younger. Strong fit for *platform shells*, not the core. |
| C# / F# | GC-based even with low-latency modes; iOS path goes through Mono/NativeAOT, fighting the runtime rather than working with it; no comparable GPU ecosystem. |
| Haskell | GC **and** laziness — unevaluated thunks make per-frame memory/timing behavior hard to reason about; almost no mobile/GPU ecosystem. Could suit an offline, correctness-critical component, not the render loop. |
| Go | Best GC story of the group (sub-ms pauses) but no zero-cost abstraction / memory-layout control; weak native iOS story; GPU work requires `cgo`, reintroducing unsafe FFI. Reasonable for an isolated subsystem (e.g. a standalone proxy), not the core. |
| Kotlin/Native | Compiles native via LLVM but still a tracing GC underneath; ecosystem strength is JVM/Android, which isn't a target platform here. |

Rust's ownership model gives deterministic drop timing, zero-cost
abstractions, and `Send`/`Sync` checked at compile time — the last of
which catches whole classes of cross-thread data races (a major real
source of Chromium/Firefox CVEs) before the code runs at all.

**Rules for `unsafe`:** `#![forbid(unsafe_code)]` in every core crate
(parser, DOM, style, layout, paint). `unsafe` permitted only in
platform-shell crates, at the smallest possible boundary, audited.
*(Current state: no crate needs it — the platform crates forbid it too,
and `scripts/check-forbid-unsafe.sh` enforces that for every crate unless
it is listed with a reason in `scripts/unsafe-exceptions.txt`, which is
empty.)*

**Platform-native code (Swift, etc.) lives only in the platform-shell
layer** (§4), where GC pauses don't touch the render loop and native OS
integration (accessibility APIs, window chrome) matters more than raw
throughput. The engine does not know these languages exist.

**JS engine:** start with **Boa** (pure-Rust JS engine — no FFI
boundary, consistent with the safety stance) rather than QuickJS
(mature but C, via `rquickjs`). Accept Boa's current spec-coverage gaps
as the cost of an end-to-end safety guarantee at this stage; write a
from-scratch JS engine later, informed by exactly where Boa falls
short. JS is meant to be a replaceable subsystem behind a runtime
interface, not load-bearing in the engine's own architecture. *(As built,
M40 / ADR 023: a page's script runs in a `script::ScriptRuntime` — Boa,
through the `js` crate, is one implementation, a stub for tests another —
and everything else, the DOM bindings included, is runtime-independent in
`script`, generated from WebIDL in `crates/script/webidl/`.)*

---

## 3. Four-layer architecture

```
┌──────────────────────────────────────────────┐
│                 Browser Shell                 │
│   tabs / windows / UI / settings / bookmarks  │
└──────────────────────┬───────────────────────┘
                        │
┌──────────────────────▼───────────────────────┐
│                 Browser Core                  │
│  sessions / permissions / profiles / storage  │
└───────────────┬──────────────────┬───────────┘
                 │                  │
        ┌────────▼────────┐  ┌──────▼──────────┐
        │    Web Engine    │  │ Network/Privacy │
        │                  │  │                 │
        │ HTML / CSS / DOM │  │ HTTP / DNS      │
        │ Style / Layout   │  │ Proxy / Routing │
        │ Paint / Compositor│ │ TLS / Isolation │
        └────────┬─────────┘  └──────┬──────────┘
                  │                  │
                  └────────┬─────────┘
                            │
                  ┌─────────▼────────┐
                  │  Platform Layer   │
                  │  GPU / OS / IPC   │
                  └───────────────────┘
```

- **Web Engine** never touches an OS or GPU API directly — consumes
  bytes, produces a display list.
- **Network/Privacy** is a peer of the Web Engine, not nested inside
  it — the engine makes requests through an opaque route (§6); it has
  no concept of "VPN," "proxy," or "tracker list."
- **Browser Core** owns permissions, profiles, storage — not a
  rendering concern, not a UI concern. *(Since M36: the `browser_core`
  crate — `Profile`, `Session`, `Settings`, `PermissionStore` — and the one
  place a production build mints the capabilities pages run with; ADR 010.)*
- **Platform Layer** implements one shared trait per OS
  (`shell_desktop` covers Linux/macOS/Windows via `winit`+`wgpu` in one
  implementation; `shell_ios`, `shell_visionos` follow the same trait).

The target source layout. *(Today the crates sit flat under `crates/`,
classified as core or platform in `scripts/core-crates.txt` /
`scripts/platform-crates.txt`; `network_privacy/` exists so far only as
the `NetworkRoute` trait, `NetworkContext` and the cookie/site policy in
the `network` crate. Since M37 it also holds the route policy, the proxy
and system-VPN routes, DNS over HTTPS and TLS trust (`network::routes`,
`doh`, `dns`, `tls`), with the filter engine in its own `filter` crate and
the fingerprint policy in `privacy`. WireGuard is evaluated, not built:
ADR 011 §5.)*

```
core/            # 100% platform-agnostic, #![forbid(unsafe_code)]
├── dom/
├── html_parser/
├── css_parser/
├── style/
├── layout/
└── paint/

network_privacy/
├── filter_engine/     # ads / trackers / AI-crawlers — one engine, three lists
├── content_classifier/# AI-generated content signals (see §6)
├── dns_privacy/        # DoH/DoT, default-on
├── proxy_relay/        # browser-only encrypted relay, no OS permission
└── route/               # NetworkRoute trait + policy resolution (see §6.1)
    ├── direct/
    ├── system_vpn/
    ├── proxy/
    ├── enterprise/
    └── user_vpn/
        └── wireguard/    # WireGuardRoute impl, wraps boringtun (replaceable)

platform/
├── shell_trait/         # Window, Input, Surface, GPU-surface-binding traits
├── shell_desktop/       # winit + wgpu — Linux/macOS/Windows, one impl
├── shell_ios/            # winit + wgpu(Metal) — iOS/iPadOS, where permitted (§7)
└── shell_visionos/       # same trait, implemented later

sandbox/                 # the renderer's OS sandbox (Landlock, seccomp) — ADR 016
renderer/                # renderer launcher + the `browser-renderer` binary

browser_core/           # profiles, sessions, settings, permissions; mints
                        # every page's capabilities (ADR 010)

app/                    # ties core + network_privacy + one shell into a binary;
                        # doubles as the renderer (`app --renderer`)
```

*(Architecture review, 2026-09-26: the layering, the message-based engine
API and the traits at the edges are the parts built to last; the DOM model,
the script bindings and absolute layout coordinates are not, and are
scheduled — `ROADMAP.md` §3, A1–A6. Since M39 the DOM is an arena of linked
nodes that every crate reads through `dom::ElementRef`, ADR 021; since M39b
a layout box is positioned in its parent's frame, so a reused subtree moves
in O(1), and a page's display list follows a layout change in part —
since M39c, a change that keeps its element's size is patched in place,
ADR 022. Since M40 script is behind a runtime interface, with bindings
generated from WebIDL and one wrapper per node, ADR 023. Since M40b tag and
attribute names are atoms — well-known ones equal everywhere, a page's own in
its document's table — interned where they are born, ADR 024.)*

---

## 4. Threading & process model

**Current implementation status (kept current, not aspirational-only):**
the model below is real. **Each page runs in a renderer process of its
own** by default (`docs/adr/016-renderer-processes.md`), **which holds
documents of one site**: a navigation that lands on another site moves the
page to another renderer, behind the unchanged API (ADR 025, M41). The host starts it
through a launcher capability it grants the engine (`app` re-launches itself
with `--renderer`), the renderer sandboxes itself (Linux: Landlock closes the
filesystem; seccomp refuses sockets, exec and ptrace), and it has no network
— every request goes to the browser process, which makes it with the
profile's cookie jar and cache, only for origins of that site the page
actually loaded, and only within the page's network grant; it keeps the
page's `localStorage` too.
The engine API is unchanged: in process mode the page's mailbox is bridged
over a bounded, fuzzed binary protocol on the renderer's stdin/stdout, and a
renderer that aborts or is killed becomes `PageCrashed`. A watchdog thread per
renderer ends one that owes a tick and makes no progress for 10 s — a runaway
script's bound (ADR 013, M38a); pages on threads have only Boa's loop and
recursion limits. A page is woken by the work it waits for (a response, a
decode, a navigation), never polled; a hidden page does no animation work; an
animating page that cannot keep up lowers its frame rate (ADR 020, M38b). `Isolation::Threads`
keeps pages on threads of one process (tests, `app --single-process`).
Inside its process (or in thread mode, inside the browser's), **each page
runs on its own OS thread**, which builds and exclusively owns
the page's `!Send` state (the arena DOM, the script runtime and its host
state, caches); the host
reaches it only through a mailbox of plain `Send` data — a bounded,
coalescing command inbox, a latest-wins frame slot, events and status
(`docs/adr/006-page-thread-actor-model.md`). **A dedicated GPU thread** in
`shell_desktop` owns the `wgpu` device and surface and draws the newest
scene; the **UI thread** only forwards input, posts ticks and hands frames
along, never waiting on a page or the GPU. A panicking page is contained to
itself and reported as `PageCrashed` (`docs/adr/007-panic-containment.md`;
`panic = "unwind"` is enforced). Page threads have a 16 MiB stack; since
Milestone 33 layout states the stack it needs for the deepest document
(`layout::MIN_STACK_BYTES`, 8 MiB) and the engine checks page threads
provide it at compile time (ADR 014). **Intra-page data parallelism** is the
shared `rayon` pool (named `worker-N`): parallel style resolution
(`style::resolve_style_tree_parallel`, results in per-worker shards),
intrinsic sizing (incremental across frames), and image decode.
**Network I/O** runs on a bounded pool of background threads
(`network::pool`: 16 workers, at most 6 requests per host, a capped queue —
`std::thread` + channels, not `tokio`; see `network`'s crate docs); every
fetch shares one `NetworkContext` (Milestone 26). A page is shown once its
document and stylesheets load; its images are decoded on the `rayon` pool
as they arrive (performance-audit fixes). The pipeline is observable (§9, Milestone
27) and sits behind one `engine` crate (Milestone 29, ADR 005). Still
single-threaded *within* a page: layout positioning (genuine sequential
dependencies — see `layout::layout_document`), display-list building, and
script. Still open: sandboxes beyond Linux, a GPU process, and a
separate decode/raster pool — this paragraph exists so "thread-safe" and
"zero-trust" stay measured claims, and is updated whenever the model
changes.

- **Main/UI thread** — event loop and orchestration only; never blocks
  on parse/layout/network.
- **Per-tab/origin worker threads** — parse, style, script execution
  isolated per tab. This is a privacy/security boundary as much as a
  performance one: a hostile page can't inspect another tab's memory.
- **Parallel style + layout** via `rayon` — selector matching and
  layout are embarrassingly parallel over the DOM tree (Servo's
  precedent). *(Style resolution: done. Layout positioning: layout
  POSITIONING has genuine sequential dependencies within a single
  document — see `layout::layout_document`'s doc comment — so "parallel
  layout" in practice means parallel MEASUREMENT/intrinsic-sizing
  passes feeding a sequential positioning pass, not parallel
  positioning itself; tracked in Phase D (`milestones/milestones-01-17.md`).)*
- **Async network I/O** via `tokio`, fully decoupled from rendering.
  *(Implemented via `std::thread` + channels instead of `tokio` — see
  status note above.)*
- **Dedicated GPU/render thread** — owns the `wgpu` device; other
  stages hand it display lists, never touch the GPU directly.
- **Decode/raster pool** — image decode, glyph rasterization, off the
  critical path.

**Isolation roadmap:** OS-process isolation per page is done (ADR 016) —
the upgrade the thread model was built not to foreclose, made when an
audit found memory corruption reachable from page content inside a
dependency (Boa; ADR 015) — and per site (ADR 025, M41): a cross-site
navigation changes process. Next: sandboxes on macOS and Windows, then a
separate GPU process. Each costs
memory and IPC; the renderer protocol sends only what changed (glyph-atlas
bands, images once) to keep it small.

**Zero-trust internally:** every IPC/thread boundary validates its
input — no trusted lengths, no trusted IDs, no trusted serialized
objects, even between the engine's own components.

---

## 5. Rendering pipeline (GPU-first)

```
DOM → Style → Layout → Display List → Scene Graph → GPU Compositor → GPU
```

- Paint never thinks in pixels — it emits an abstract display list
  (rects, glyph runs, images, clips, transforms).
- Retained-mode rendering: compositing layers (scroll containers,
  opacity, transforms) are separate GPU render targets composited
  together — cheap animations without re-running layout.
- Text: rasterize each glyph once into a GPU texture atlas, then draw
  textured quads — standard modern-renderer approach. *(As built, M35 /
  ADR 009: `ttf-parser` parses, `rustybuzz` shapes, our own safe
  rasterizer draws; fonts come from a host-granted source, served to
  renderer processes over IPC.)*
- `wgpu` unifies Metal / Vulkan / DX12 / GL behind one safe Rust API —
  the actual technical case for Rust over Swift given the
  cross-platform requirement (§2).
- **Power budget as an architectural concept**: a page must not be able
  to peg the GPU/CPU indefinitely — explicit frame-budget and
  thermal-state awareness, not just "GPU-first" as a slogan.

> **As built (Milestone 34, ADR 017).** Clips, transforms and opacity are
> `Push*`/`Pop*` markers in the display list; one walk (`rendering::walk`)
> gives every renderer each item's world transform and clip. Transform and
> scroll layers are *not* separate render targets: every instance carries
> its matrix and clip, so they cost nothing to draw, and a transform
> animation or a container scroll updates the layer's matrix in place — no
> cascade, layout or display-list build. Opacity groups are offscreen
> targets composited at their opacity. The GPU keeps a back buffer and
> redraws only a frame's damage. Tiled layer rasterization is not needed
> yet; it becomes worth it when a page has many large promoted layers.

---

## 6. Privacy Boundary

Every category of data crossing from engine/browser to a website (or
vice versa) gets an explicit, named policy — privacy as an architecture
property, not a features list:

| Crossing the boundary | Policy type |
|---|---|
| IP address | Network policy (route, §6.1) |
| Cookies | Storage policy |
| Fonts, GPU info, canvas/WebGL readback | Fingerprint policy — clamp/noise, not raw passthrough |
| Camera, location, clipboard | Permission policy — scoped, revocable, auditable |
| Storage (IndexedDB, cache, service workers) | Origin/partition policy |

*As built (M37; ADR 011, 012, 018, 019):* each row has a named policy and a
proving test — the table is in `milestones/milestone-37-privacy-layer.md`,
with the rows M37 adds (host names via DoH, time and timezone, ads and
trackers, header identity, TLS trust).

**Curated API surface ("no API slop"):** implement what real sites
need (fetch, DOM, canvas 2D, WebGL, service workers...); deliberately
skip the long tail of exotic hardware-access APIs by default — every
exposed API is both attack surface and fingerprinting surface.

### 6.1 Network routing — first-class, not self-operated

**We do not build or operate our own VPN service.** (Standing
principle — see §1, Vendor-independent.) The engine has first-class
*support* for routing traffic through whatever the user chooses, via
an abstraction the Web Engine never sees:

```
Web Content → Fetch API → Network Service → NetworkRoute
                                                  │
                        ┌──────────┬─────────────┼───────────┬──────────────┐
                        ▼          ▼             ▼           ▼              ▼
                    DirectRoute SystemVPNRoute ProxyRoute UserVPNRoute EnterpriseRoute
                                                             │
                                                        WireGuardRoute
                                                             │
                                                         boringtun
```

- **`NetworkRoute` is a trait, not a VPN concept.** WireGuard is one
  *implementation* reachable through it (`UserVPNRoute` →
  `WireGuardRoute` → `boringtun`), not part of the browser's
  networking model. `boringtun` is a **replaceable dependency** behind
  that trait — swappable later without touching Network Service or the
  engine.
- Built-in WireGuard client (pure-Rust `boringtun`, no unsafe FFI,
  production-proven — Cloudflare's own implementation) so a user can
  import *any* provider's config — Mullvad, ProtonVPN, a self-hosted
  server, whatever — and it just works. We never see or route their
  traffic ourselves; we ship no server, no infrastructure, no VPN
  service of our own.
- Optional **browser-only encrypted relay** (à la Private Relay /
  Brave's forward proxy) for a lighter default-on option needing no
  OS-level TUN permission — still someone else's infrastructure, or an
  explicitly separate, independently-operated partner if we ever want
  a zero-config default (the Firefox/Mullvad pattern), never a service
  we run ourselves.
- **Routing is a policy dimension**, not just a per-request choice —
  the architecture should not prevent (even if the UI doesn't initially
  expose) rules like a profile defaulting to a given route, a specific
  origin routing differently, or Private Browsing forcing `UserVPNRoute`.
  Whenever this is actually built, define an explicit precedence order
  (origin rule vs. profile default vs. mode override) — an interface
  that "allows" policy composition without a defined resolution order
  just defers the ambiguity to runtime.
- Ad/tracker/AI-crawler blocking sits in `filter_engine/` at the
  network-interceptor layer, one engine matching three list categories
  (EasyList-format ads, EasyPrivacy-format trackers, AI-crawler
  user-agents/domains) — always active, engine-level, not an
  extension's responsibility.
- AI-generated-content signals (`content_classifier/`) run locally,
  never phone out to a cloud detector: provenance metadata (C2PA) as
  the highest-confidence signal, known-model watermark detection where
  applicable, and a clearly confidence-scored heuristic layer for
  everything else — surfaced as a badge, never a silent, overconfident
  block.

**We do not claim a VPN provides anonymity.** A route changes network
identity — it can hide source IP from a destination. It does nothing,
by itself, about storage identity, tracking identity, fingerprinting
identity, or account identity. Those are separate rows in the §6
Privacy Boundary table, each with its own mechanism; a VPN is one
route option addressing exactly one of them, never marketed or coded
as a stand-in for the rest.

**Trust-minimization note for later:** if a multi-hop relay is ever
built for stronger privacy properties, it only works if the hops have
*independent, non-colluding operators*. Two servers we run ourselves is
single-operator trust with extra latency, not the privacy property
multi-hop is supposed to buy.

---

## 7. Extensions — a browser-shell concern, not an engine concern

- **Engine (`core/`) exposes hook points**, not an extension concept:
  intercept/modify network requests, inject/read DOM, inject scripts
  into an isolated context, register UI panels.
- **Browser shell implements the extension system** on top: manifest
  format, permission model, isolated JS runtime, install/manage UI.
- Because ad/tracker/AI-crawler blocking already lives at the engine
  level (not as an extension permission), we sidestep the
  Manifest-V3-style problem where weakening a powerful extension API
  (to prevent it doubling as spyware) also weakens ad blockers. Worth
  deciding *consciously*, if/when third-party extensions get the same
  blocking power, rather than defaulting into either extreme.

---

## 8. Platform distribution reality (not an architecture constraint)

The engine is designed identically regardless of where it's currently
legal to ship it. Distribution is a separate, revisitable concern:

| Platform | Status (as of Aug 2026) |
|---|---|
| Linux, Windows, macOS | Full engine, no restriction |
| Android | Full engine, no restriction |
| iOS/iPadOS — EU | Permitted since iOS 17.4, under DMA Art. 5(7) |
| iOS/iPadOS — Japan | Permitted since iOS 26.2 (Dec 2025), under the Mobile Software Competition Act |
| iOS/iPadOS — elsewhere | Not permitted; Apple requires WebKit |

Known friction even where permitted: Apple geofences third-party-engine
*developer testing* to devices physically in the EU (no equivalent
carve-out for outside developers), and hasn't confirmed security-update
continuity for EU users traveling 30+ days. Apple's own entitlement
docs describe providing JIT and multiprocess support to authorized
alternative-engine developers.

**Explicit decision: no WebKit fallback, no engine compromise to
satisfy App Store policy.** Where a platform doesn't permit the engine,
it doesn't ship there yet — a distribution gap, not a reason to
compromise the architecture. Nothing about the engine needs to change
if/when the rules change elsewhere.

---

## 9. Engineering practices (build in, don't retrofit)

- **Fuzzing** on every parser (HTML tokenizer, CSS, URL, font,
  image, network protocols, IPC/serialization) from the start —
  browsers are unusually good fuzzing targets and unusually risky ones
  to skip it on.
- **Determinism / reproducibility** — ability to snapshot
  (URL + HTML + CSS + network responses + config + viewport) and replay
  a rendering bug exactly.
- **Observability** — internal tracing/metrics/crash reporting from
  day one (`about:engine`-style diagnostics eventually); this is not
  optional once the pipeline is multi-threaded — that's precisely when
  debugging without it becomes miserable. *(Baseline shipped in
  Milestone 27: `diagnostics` crate — `tracing` facade, security
  `Guard`s, metrics; local-only, no telemetry. `about:engine` and crash
  reporting are still open.)*
- **Text subsystem isolated early** — Unicode, bidi, shaping, line
  breaking, font fallback, variable fonts. Treat as its own major
  subsystem, not "draw a string."

---

## 10. Where the implementation is

Milestone 1 — hardcoded HTML rendered as GPU boxes in a real window —
was the first target and has long been met. `ROADMAP.md` is the plan
(what is achieved, what comes next, and why); `AUDIT.md` is the status
of every milestone and principle, checked against the code — together
they track exactly the gap between §1 of this document and the code.
Keep the status note in §4 current as the threading model changes.
