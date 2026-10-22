# ADR 004 — `Origin` and `Site`: one type each, one same-origin rule

- **Status:** accepted (Milestone 28)
- **Decides:** how the engine represents "who is this?" for security and privacy policy, and where the same-origin and same-site rules live.
- **Constrains:** cookies (`SameSite`), `localStorage`/`sessionStorage` keys, HTTP-cache partitioning, `Referer`, the `fetch()`/XHR same-origin check, and later capabilities (M36), third-party blocking (M37) and frames.

## Context

Before this milestone "origin" was a `String` built by `network::origin_of`
(`"https://example.com"`), compared with `==` inside the `js` crate, while
`Referer` used the `url` crate's own origin comparison and cookies/cache used a
third notion, the schemeless registrable domain from `network::site`. Three
implementations of one security rule is how rules drift apart. Worse, a document
with no URL (the built-in page, a synthetic test document) had
`page_origin: None`, which the `fetch()` check treated as "enforce nothing", and
whose requests carried no initiator, which the cookie jar treated as
"browser-initiated, never cross-site". A page with no address was the *most*
privileged kind of page.

## Decision

1. **A new core crate, `origin`** (no dependencies but `url`; `forbid(unsafe_code)`),
   owns the Public Suffix List (moved from `network::site`, ADR 002 unchanged
   otherwise) and three types:
   - `Origin` — either a **tuple** (scheme, host, effective port — default ports
     filled in, so `https://a.com` and `https://a.com:443` are the same origin) or
     **opaque**. Only `http`, `https`, `ws` and `wss` produce tuple origins; a
     `blob:` URL has the origin of the URL inside it; every other scheme
     (`data:`, `file:`, `about:`, `javascript:`) and every unparseable string is
     opaque. **Each opaque origin is unique**: it equals only itself (a
     process-unique id), never another opaque origin, so two `data:` documents
     can never share storage or pass a same-origin check. `file:` is opaque on
     purpose: a local file must not be same-origin with every other local file.
   - `Host` — a lowercase, punycode-normalised domain, or an IPv4/IPv6 address.
     The `url` crate does the normalisation; the public API does not expose `url`
     types, so the parser stays replaceable.
   - `Site` — **schemeful**: (scheme, registrable domain) for a domain host,
     (scheme, address) for an IP, and opaque for an opaque origin (unique, equal
     only to itself). Schemeful because an `http://` page is not the same party
     as its `https://` counterpart for `SameSite` purposes — a network attacker
     controls the first. This is the modern definition (RFC 6265bis
     "same-site", HTML "schemefully same site").
2. **One same-origin rule** (`Origin::same_origin`) and **one CORS read check**
   (`origin::cors_response_readable`) live in this crate. `network::origin_of`
   and the string comparison in `js` are deleted, and `Referer` uses
   `Origin::same_origin` too.
3. **No origin means an opaque origin, never "no enforcement".**
   `JsEnvironment` is built from an `Origin`; a page with no URL gets a fresh
   opaque one. Its requests carry that opaque origin as their initiator, so they
   are cross-site to everything (`SameSite=Lax`/`Strict`/unspecified cookies are
   withheld from its subresource requests) and their responses are readable only
   with `Access-Control-Allow-Origin: *`.
4. **`Access-Control-Allow-Origin: null` is not honoured.** The spec lets an
   opaque origin read a response that allows `null`, but every opaque origin
   serialises to `null`, so allowing it would let any sandboxed or `data:`
   document read it. Zero-trust (§1) wins over the letter of the spec here.
5. **Requests carry an `Initiator` (origin plus optional URL)**, not a URL
   string. The origin decides cookies and cache partition; the URL, when there
   is one, is only for `Referer`.
6. **Storage is keyed by `StorageKey { top_level_site, origin }`.** Today
   `top_level_site` is always the origin's own site (there are no frames); the
   key already has the shape frames will need, so adding them does not change
   the storage layer. Opaque origins never get a shared area.

## Consequences

- Frames and `postMessage` can be added without changing these types (a frame's
  storage key is its own origin under its top-level document's site).
- Behaviour changes, deliberately: the built-in demo page can no longer read
  cross-origin responses without CORS; a trailing-dot host (`example.com.`) is a
  different *origin* from `example.com` (as in browsers) but the same *site*;
  `http://a.com` and `https://a.com` are now different sites, so a `Lax` cookie
  set over https is withheld from an http page's subresource request to it.
- `Origin::parse` is fuzzed (`fuzz/fuzz_targets/origin_parse.rs`), as is the PSL
  parser (`psl_parse`, moved with the list).
