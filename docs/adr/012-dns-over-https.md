# ADR 012 — DNS over HTTPS, with no default vendor

- **Status:** accepted (Milestone 37, 2026-10-11)
- **Decides:** how the browser resolves host names privately, and who
  chooses the resolver.
- **Constrains:** `network` (`dns` module, the DoH resolver),
  `browser_core` settings (`dns.doh`), hosts.

## Context

Plain DNS shows every host a user visits to their network and resolver.
DNS over HTTPS (RFC 8484) hides it from the network — but sends it to the
DoH operator instead. Architecture doc §1 (vendor-independent): no single
company may be architecturally necessary, so the browser cannot pick one
operator for everyone. The hardening pass recorded this as a product
decision, not just code.

## Decision

1. **DoH is off until the user names a resolver** (setting `dns.doh`, flag
   `--doh`). There is no built-in default operator. The documentation lists
   presets from several independent operators (the user chooses); none is
   preferred in code.
2. **Strict when on.** With a resolver configured, a name that DoH cannot
   resolve fails the request; there is no fallback to system DNS (which
   would leak exactly what DoH hides). A user who wants fallback must turn
   DoH off.
3. **Bootstrap.** The resolver's own host must be reached somehow: a
   resolver URL whose host is an IP literal needs nothing; otherwise the
   configuration may give bootstrap addresses; otherwise the resolver's
   host name alone is resolved by the system (leaking only that the
   browser uses this resolver).
4. **HTTPS only**, except a resolver on a loopback address (a local
   resolver, and tests) may use plain HTTP.
5. **Implementation:** RFC 8484 `GET` with `?dns=` (base64url, no padding),
   `A` and `AAAA` queries, `application/dns-message`. The DNS wire format is
   encoded and decoded by hand (`network::dns`): bounded (512-byte names,
   compression pointers followed at most 16 times, 64 answers), never
   panics, fuzzed (`dns_message`). Answers are cached for their TTL, capped
   at 5 minutes and 1,024 names. IP-literal hosts and `localhost` are never
   sent to the resolver.
6. Resolution plugs into the transport (`ureq`'s resolver hook), so every
   route that connects directly uses it; a proxy resolves names itself
   (ADR 011).

## Consequences

- The DoH resolver's queries go through their own agent, without cookies,
  `Referer` or the page's identity.
- `network` fails a request with `NetworkError::Transport("DoH: …")` when
  DoH cannot answer.
