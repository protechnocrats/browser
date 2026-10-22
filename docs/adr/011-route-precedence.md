# ADR 011 — Network routes and their precedence

- **Status:** accepted (Milestone 37, 2026-10-11)
- **Decides:** which `NetworkRoute` a request takes, in what order the
  rules that choose it apply, what a route does when it cannot connect, and
  which routes exist.
- **Constrains:** `network` (`RoutePolicy`, `ProxyRoute`, `SystemVpnRoute`,
  the shared transport), `browser_core` (where the policy is configured),
  hosts (the VPN probe), and every route added later (WireGuard).

## Context

Architecture doc §6.1: routing is a policy dimension, and "an interface
that allows policy composition without a defined resolution order just
defers the ambiguity to runtime". Until M37 there was one route,
`DirectRoute`, and `NetworkContext::with_route` took one route for a whole
profile.

## Decision

1. **A `RoutePolicy` is itself a `NetworkRoute`**, wrapping named routes.
   For each request it picks exactly one, in this order — the first that
   applies wins:
   1. **Private-browsing override** — when the profile is in private mode
      and an override route is set, every request takes it.
   2. **Per-site rule** — a rule for the request's **top-level site**: the
      site of the page that made it (a subresource or script request), or
      the destination's site (a navigation). Not the destination of each
      subresource: a page's every request goes the same way, so a page
      routed through a proxy cannot leak its visitor's address through a
      third-party image. Rules match a site exactly or as a registrable-
      domain suffix; the most specific rule wins.
   3. **Profile default** route, if set.
   4. **`DirectRoute`.**

   `RoutePolicy::choose(url, ctx)` is a pure function of the policy and the
   request, unit-tested for every combination of the three overrides.
2. **A route never falls back.** A proxy that cannot be reached, or a system
   VPN that is down, is an error for that request — never a direct
   connection. Falling back would turn "the proxy is down" into "the user's
   address leaks". The error says which route failed.
3. **Every route shares the profile's store** (cookie jar, HTTP cache,
   filter, request policy). Routes differ only in how bytes reach the
   server: `DirectRoute`'s fetch logic (hop-by-hop redirects, cookies,
   cache, filter, third-party rules) runs for every route over a transport
   that is direct, through a proxy, or gated on a VPN.
4. **Routes built in M37:**
   - `ProxyRoute` — HTTP (`CONNECT`) and SOCKS4/4a/5 proxies, with
     optional credentials, through `ureq`'s proxy support. SOCKS needs
     `ureq`'s `socks-proxy` feature, which adds the `socks` crate (0.3:
     ~1,000 lines, MIT/Apache-2.0, `byteorder` and `libc`, contains
     `unsafe` for socket options; unmaintained since 2021). Accepted for
     now because Tor and SSH tunnels speak SOCKS; it sits behind the route
     trait and goes with `ureq` 3's own connector API.
   - `SystemVpnRoute` — direct connections, made only while a host-supplied
     probe says a system VPN is up (on Linux, `app` checks for a `tun`,
     `wg` or `ppp` interface that is up). The engine reads no system state
     itself (capability-based: the probe is granted by the host).
5. **WireGuard: evaluated, not built.** A browser-only WireGuard route needs
   a user-space TCP/IP stack on top of `boringtun` (e.g. `smoltcp`) — no
   operating-system TUN, which needs privileges. That is a large, separate
   piece of work; the route trait is what it plugs into, and nothing above
   it changes. We ship no VPN service either way (architecture doc §1).

## Consequences

- `NetworkContext::with_config` builds a context from a `NetworkConfig`
  (routes, policy, transport settings); `NetworkContext::new()` is a direct
  route with the defaults.
- A route choice is visible as an `engine::network` event field (`route`).
- DNS: a proxied request is resolved by the proxy (HTTP `CONNECT` and
  SOCKS4a/5 send the host name); a direct one by the system or by DoH
  (ADR 012).
