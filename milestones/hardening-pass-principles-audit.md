# Hardening pass — closing the principles audit

> **Status:** Done (between Milestones 27 and 28). **Still open:** see *Not fixed* below; each item was taken up by M34–M38 (M38 as 38a and 38b) — what remains is listed in `AUDIT.md`.

**Goal:** fix the gaps a principle-by-principle audit of the code found (against
`browser-engine-architecture.md` §1), everything that could be fixed *and proven* in
one pass; and state plainly what could not. Done after Milestone 27, pulling
forward pieces of M28 (site/cookie model), M36 (file capability), M37 (request
hygiene, cache partitioning) and M38 (script limits) that were cheap and urgent.
Decision records: ADR-002 (Public Suffix List), ADR-003 (linear-time patterns).

## What the audit found, and what happened

| # | Finding (measured) | Principle | Status |
|---|---|---|---|
| 1 | A network-fetched page made the engine decode a **local file** (inline `style` / script-set `src` → `ImageCache::load_from_file`); `url(/dev/zero)` would read until OOM | capability, zero-trust | **Fixed.** `ImageCache` has no file access by default (`FileAccess::Denied`); regular files only, ≤ 64 MiB even when granted; refusals are a `Guard` event, once per path. End-to-end regression through `fetch_page` + layout. |
| 2 | `while(true){}` hung the UI thread forever; `localStorage` unbounded (64 MiB stored in 2.9 s); script errors in callbacks silently swallowed | zero-trust, observable | **Partly fixed.** Loop-iteration limit (10 M) and recursion limit pinned; not catchable by `try/catch`; 5 MiB storage quota (`QuotaExceededError`); every callback error now reported. **Residual:** see "Not fixed" 1. |
| 3 | `pattern="(a+)+$"` + 24-char value froze the cascade 0.8 s, 28 chars 4.7 s — per frame | zero-trust | **Fixed.** Linear-time `regex` engine in a shared `pattern` crate; bounded compile; backtracking-only patterns ignored and reported. The audit's exact input is now instant, 200 000-char hostile values too. |
| 4 | `Domain=com` / `Domain=co.uk` cookies accepted and sent to unrelated sites; `SameSite` ignored; host-only cookies leaked to subdomains; third-party subresources carried cookies | privacy-first, zero-trust | **Fixed.** Vendored Public Suffix List (10,325 rules); host-only scoping; `SameSite` (`Unspecified` enforced as `Lax`) evaluated per redirect hop against the request's initiator; `Secure` needs https; `__Secure-`/`__Host-` prefixes; `SameSite=None` needs `Secure`; persistence format v2 (v1 still loads). |
| 5 | Requests sent `User-Agent: ureq/2.12.1`, no `Accept-Language`, no policy for `Referer`; HTTP cache unpartitioned | privacy-first | **Fixed.** Frozen `browser-engine/0.1` UA and a fixed `Accept-Language` (configurable per context); `Referer` = strict-origin-when-cross-origin, never with credentials/fragment, never https→http; HTTP cache keyed by top-level site. |
| 6 | `localStorage` per page instance (neither origin-keyed nor surviving navigation) | privacy-first | **Fixed** (in memory). One area per origin, shared by the profile; opaque origins never share; `sessionStorage` per page; bounded map. |
| 7 | GPU backend drew all rects, then all glyphs, then all images: paint order lost; image order from `HashMap` iteration | GPU-first, deterministic | **Fixed in logic, unverified on hardware.** `rendering::plan_draw_batches` (pure, 8 tests incl. a tiling property test); backend executes it in order, one `draw` per run. |
| 8 | GPU-init failure (no adapter, no surface) panicked; `Page::new` panicked on a bodyless document | robustness | **Fixed.** `GpuRenderer::new` → `Result<_, GpuError>`; `shell_desktop::run` → `Result<_, ShellError>`; `main` reports and exits 1. |
| 9 | Mutex-poison handling inconsistent (`style`/`layout` re-panicked; `network` tolerated) | thread-safe | **Fixed.** All recover the data. |
| 10 | Platform crates *allowed* `unsafe` although they use none | memory-safe | **Fixed.** `gpu_backend` and `shell_desktop` now `forbid(unsafe_code)`; the guardrail covers every crate unless listed in `scripts/unsafe-exceptions.txt` (empty). |
| 11 | Full-precision rAF timestamps; the frame clock read inside the pipeline | privacy-first, deterministic | **Fixed.** Timestamps coarsened to 1 ms; `build_frame` takes an injected `now` — a fixed clock reproduces a frame. |

## Bugs found *while fixing* (by the M25 fuzz harness, extended for this pass)

- **The new suffix lookup was quadratic in label count** — a 100,000-label hostname
  in an `<img src>` took 66 s, 1 MiB took 433 s (a web-content-reachable freeze).
  Found by the harness's structural stress inputs within minutes of writing the
  code. Fixed: hosts beyond DNS limits (253 B / 127 labels) are refused before any
  work, and the lookup is allocation-free and linear. Regression tests included.
- Two existing tests pinned the *old, wrong* behaviour and were updated deliberately:
  "host-only cookies match subdomains" and "rAF timestamps are fractional".

## Not fixed — and why

1. **A total script-execution budget.** Boa 0.17 counts loop iterations per loop
   *instance* and has no interrupt hook, so nested loops multiply and pure compute
   is unbounded. Closing this needs an engine upgrade or a killable script
   context (ROADMAP M38 / ADR-013). Accidental and simple hostile cases
   are handled; a determined adversary is not stopped.
2. **Compositing, clipping, transforms, layers, dirty tracking, `rotate()`/`skew()`
   rendering** — the display list still has three item types and the pipeline
   reruns per redraw. Multi-milestone (M31/M34), needs shader work that cannot be
   verified here.
3. **Thread-safety architecture** — page-per-thread ownership, a GPU thread,
   `Rc<RefCell>` → `Send` state (M29/M30). The largest single open item.
4. **Capability layer, Browser Core, `network_privacy/*` (filter engine, DNS
   privacy, proxy/relay, route policy), extension hooks** (M36/M37). Two of these
   need a *product decision*, not just code: a DNS-over-HTTPS resolver cannot
   default to any one vendor (§1), so it needs a user-supplied or documented list.
5. **Third-party cookie *blocking* and top-level-site storage partitioning.**
   `SameSite` now withholds `Lax`/`Strict`/unspecified cookies cross-site, but a
   `SameSite=None; Secure` cookie still rides third-party requests (that is M37
   policy). `localStorage` is per-origin, not yet per (top-level site, origin) —
   there are no frames to embed a third party in. Not persisted to disk (M36).
6. **Accessibility bridge** (`accesskit`) — a separate platform project; the tree
   is built but nothing consumes it.
7. **Frame budget / power** — no throttling of hidden windows (M38).
8. **Dependencies** — Boa 0.17's two advisories (documented, expire 2026-12-31).
9. **Full deterministic replay** — the cascade clock is injected, but `main.rs`
   still feeds `Instant::now()` and the rAF start time; recording/replaying input
   is M38.

## Unverified

- **The GPU backend change has not run on a GPU** (no window/GPU in this
  environment); it compiles and the ordering logic is unit-tested. It needs a real
  look at the demo page on hardware before it is trusted.
- The libFuzzer runs and the new CI job steps remain unexecuted (as in M25);
  9 targets are discovered by `cargo fuzz list`, and the stable-Rust replay,
  stress and 94,500-input mutation suites pass.
- `UA: browser-engine/0.1` may be blocked or served differently by some sites; it is
  a policy (`NetworkPolicy`), one line to change per profile.
