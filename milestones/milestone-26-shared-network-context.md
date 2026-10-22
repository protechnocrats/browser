# Milestone 26 — Shared network context

> **Status:** Done. **Still open:** the known gaps below are small; route policy attaches to `NetworkContext::with_route` in **M37** (done there, ADR 011).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** one cookie jar, one HTTP cache, one route per *profile* instead of
one per call site. Fixes a real correctness bug and is the seam origin
partitioning (M28) and routing policy (M37) attach to.

## The bug
Every `fetch()`, every `XMLHttpRequest`, and every page load built its own
`DirectRoute` — its own jar and cache. A cookie set by a page load was
invisible to the page's own later `fetch()`. Separately, `ureq` followed
redirects internally, so an intermediate hop's `Set-Cookie` was never seen at
all, and the jar was keyed only off the *original* request host.

## In scope (all done)
- `network::NetworkContext`: a cheap `Clone`, `Send + Sync` handle over a
  `NetworkStore` (cookie jar, HTTP cache, optional persistence path) and an
  `Arc<dyn NetworkRoute>`. The store is route-independent on purpose: a
  proxy route (M37) needs the same jar and cache.
- `NetworkRoute` gains `Send + Sync`; `NetworkContext::with_route(|store| …)`
  builds a context around any route. `DirectRoute::with_store` is the only
  constructor and `NetworkContext::new()` its only production caller —
  `DirectRoute::new()` no longer exists.
- Threaded through `JsEnvironment::new_with_network`, `app::fetch_page`,
  `app::navigate_async` and `app::Page` (a `network` field). `JsEnvironment::
  new`/`new_with_page_url` remain as test/demo conveniences with a private
  context, documented as such.
- Redirects are followed hop by hop: each hop sends only its own host's
  cookies and stores `Set-Cookie` under its own host — including on the 302
  itself. A redirect to a non-http(s) scheme (`file:`, `data:`, …) is
  refused; a chain is capped at 10 hops; a malformed `Location` is an error.
- Opt-in cookie persistence: `NetworkContext::load_cookies(path)` (missing
  file fine), rewritten atomically after every `Set-Cookie`, owner-only
  (0600) on Unix, session cookies never written. `app --cookie-jar <path>`.
- Lock poisoning is recovered rather than propagated, so one page thread's
  panic while holding the jar cannot make every other page's request panic
  (M30 relies on this).

## Out of scope
Per-origin partitioning and `SameSite`/`Secure`/host-only rules (M28),
third-party rules and proxy/VPN routes (M37).

## Acceptance criteria
- [x] `grep -rn "DirectRoute::new()" crates app` finds nothing; the single
      production construction is `NetworkContext::new()`.
- [x] Server sets a cookie on the page load; the page's own script `fetch()`
      sends it (`app::tests::a_cookie_set_by_the_page_load_is_sent_by_the_
      pages_own_script_fetch`, through the real `fetch_page` → `Page::new` →
      JS path), and the next navigation carries it. Also at the JS level
      across separate environments, for both `fetch()` and `XMLHttpRequest`.
- [x] Redirect A→B: a cookie set by B is scoped to B, A's (set on its 302) to
      A, and A's is not sent to B (A=`127.0.0.1`, B=`localhost`).
- [x] Two `NetworkContext`s share neither cookies nor HTTP cache.

## Tests added (network 44→60 here, 65 after M27; js +1; app +2)
Redirect statuses 301/302/303/307/308 with relative, root-relative and
absolute `Location`; loop bounded; non-web-scheme redirects refused (4
schemes); malformed and missing `Location`; persistence round trip, off by
default, session cookies never written, 0600 permissions, corrupt file
loads without panicking; custom route swappable for both sync and async
fetch; lock-poison recovery; `NetworkContext`/`NetworkStore`/`DirectRoute`
are `Send + Sync` (compile-time).

## Behaviour changes worth knowing
- `FetchResponse::final_url` is now the last hop's URL as `fetch` computes it
  (same value `ureq` reported before).
- The HTTP cache is still keyed by the *requested* URL (unchanged); M37
  makes it partitioned.
- Cookie scoping is still host-keyed with the existing simplifications
  (host-only cookies match subdomains, `SameSite` ignored) — those are M28.

## Known gaps
No global `NetworkContext` accessor, deliberately: callers are handed one,
which is what M36's capability layer needs.
