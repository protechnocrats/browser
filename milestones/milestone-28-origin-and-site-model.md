# Milestone 28 — Origin & site model

> **Status:** Done. **Still open:** frames (none exist yet) → backlog; ~~the privacy policy on top of `Site` (third-party blocking) → M37~~ — done in M37 (ADR 019).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** a first-class `Origin` and `Site` used everywhere a security or
privacy rule asks "who is this?", with one same-origin rule instead of
three, and no document that escapes policy by having no URL.

**Builds on:** Milestone 26 (`NetworkContext`), and the hardening pass,
which had already delivered the Public Suffix List, host-only cookies,
`SameSite`, `Secure`/prefix rules and origin-keyed `localStorage` (see
`milestones/hardening-pass-principles-audit.md`). Decision record:
`docs/adr/004-origin-and-site-types.md`.

## What was built

- **New core crate `origin`** (depends only on `url`, forbids `unsafe`,
  listed in `scripts/core-crates.txt`):
  - `Origin` — tuple (scheme, normalised host, effective port) or opaque.
    Only `http`/`https`/`ws`/`wss` give tuple origins; `blob:` takes its
    inner URL's origin; `data:`, `file:`, `about:`, `javascript:`, `ftp:`
    and anything unparseable are opaque. Every opaque origin is unique
    (a process-unique id), so it equals only itself. Input over 2 MiB is
    opaque without being parsed.
  - `Host`, `Scheme`, and `Site` — the **schemeful** registrable domain
    (`https://example.co.uk`), or opaque.
  - `StorageKey { top_level_site, origin }` — frames-ready storage keys.
  - `cors_response_readable` — the single rule for script reading a
    response. `Access-Control-Allow-Origin: null` is never honoured.
  - The Public Suffix List moved here from `network::site`, with its
    vendored data file (`git mv`, so history follows) and scripts.
- **`network`:** `network::origin_of` and `network::site` are gone.
  `RequestContext` now carries an `Initiator { origin, url }` instead of a
  URL string, plus a **credentials mode** (`Include`, `SameOrigin`,
  `Omit`), evaluated per redirect hop: under `SameOrigin`, a cross-origin
  hop sends no cookies and its `Set-Cookie` is ignored. Cookie `SendContext`
  compares `Site` values. `Referer`'s same-origin test uses `Origin`. The
  HTTP-cache key is (top-level site, credentialed or not, URL).
- **`js`:** `JsEnvironment` is built from an `Initiator`. A document with
  no URL gets a fresh opaque origin: it can read only responses that say
  `Access-Control-Allow-Origin: *`, its requests are cross-site to
  everything, and its storage is private. `fetch()` defaults to
  `credentials: 'same-origin'` and accepts `'include'`/`'omit'`;
  `XMLHttpRequest` follows `withCredentials`. Reading a credentialed
  cross-origin response needs an exact `Access-Control-Allow-Origin` plus
  `Access-Control-Allow-Credentials: true`. The check is made against the
  response's **final** URL, so a same-origin URL that redirects elsewhere
  does not launder the other origin's response. Relative `fetch()`/XHR
  URLs now resolve against the page URL. `localStorage` is keyed by
  `StorageKey`.
- **`app`:** navigations carry the initiating page's `Initiator`. A link
  click on the built-in page used to go out as a typed navigation carrying
  even `SameSite=Strict` cookies; it is now a cross-site navigation from an
  opaque origin.
- **Fuzzing:** new target `origin_parse` (10 targets now), which also
  asserts the properties policies rely on: same-origin implies same-site,
  an origin is same-origin with itself, and a tuple origin's serialisation
  parses back to the same origin. `url_origin`, `cookie_header` and
  `psl_parse` were moved onto the new API.

## Acceptance criteria

- [x] `Origin`/`Site` unit tests including adversarial URLs: userinfo
      (`https://good.com@evil.com/`), backslashes (`evil.com\@good.com`),
      mixed case, trailing dots, IDN homographs (Cyrillic `а`), IPv4/IPv6
      spellings, default ports, `file:`/`data:`/`blob:`.
      (`crates/origin/src/lib.rs`, 17 tests.)
- [x] Cookie tests: `SameSite=Strict` not sent cross-site; `Lax` only on
      top-level navigations; `Secure` rejected over http;
      `Domain=co.uk`/`Domain=com` rejected (hardening pass, still passing),
      plus opaque initiators and schemeful sites
      (`an_opaque_initiator_is_cross_site_to_everything`,
      `sites_are_schemeful_for_samesite`).
- [x] Two pages on different origins do not see each other's
      `localStorage`; same origin does (`js` and `app` tests).
- [x] `network::origin_of` and the string comparison in `js` are gone:
      `grep -rn "origin_of\|\.origin()" crates app fuzz/src` finds nothing.
- [x] Fuzz targets for the PSL parser (`psl_parse`) and `Origin::parse`
      (`origin_parse`); the stable replay/stress/mutation suite passes.

Standing Definition of Done: `cargo build`/`test --workspace --release`
clean (706 tests, 0 failures, up from 684); clippy unchanged at 44;
dependency-direction and forbid-unsafe checks pass; ADR 004 written before
the code.

## Behaviour changes, on purpose

- The built-in demo page can no longer read cross-origin responses without
  CORS, and its requests no longer carry `SameSite`-restricted cookies.
- `http://a.com` and `https://a.com` are different sites.
- A cross-origin `fetch()`/XHR no longer sends cookies unless it asks to.
- Five `js` tests that read local-server responses from a no-URL
  environment were changed to load their page from the server's origin —
  they had been relying on the "no origin, no enforcement" hole.

## Out of scope (unchanged from the plan)

- Frames and `postMessage` — the types are frames-ready
  (`StorageKey::embedded`), nothing else is built.
- CORS preflight and non-GET methods (the network layer is GET-only), and
  `Vary`-aware caching.
- Third-party cookie *blocking policy* — Milestone 37; this milestone
  provides the site data it needs.
