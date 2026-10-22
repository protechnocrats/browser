# Milestone 37 — Privacy layer

> **Status:** Done, with gaps (2026-10-11). **Still open:** a WireGuard
> route is evaluated, not built (ADR 011 §5) → backlog; ~~extra CA roots are
> tested for loading and fail-safety but not against a live TLS server with
> a private CA~~ (done in the gap pass: `network/tests/private_ca.rs`); the corpus does not exercise filtering (it replays recorded
> responses, below the filter); no third-party filter list ships (they go
> stale, ADR 019 §1) — users load their own; `SOCKS` support adds an old
> dependency (`socks` 0.3) → replace with `ureq` 3's connector API. Decision
> records: ADR 011 (routes), ADR 012 (DoH), ADR 018 (certificate trust),
> ADR 019 (filter, third parties, fingerprinting). Overall status:
> `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** every category of data crossing the privacy boundary
(architecture doc §6) has a named, tested policy. Fixes P5 (`Date`), P6
(third-party cookies), P7 (no filter, routes or DNS privacy) and P8 (trust
store).

## The privacy boundary, as built

The table the acceptance criteria ask for: each row of architecture doc §6
(and the rows M37 adds), its policy, and the tests that prove it.

| Crossing the boundary | Policy | Proved by |
|---|---|---|
| **IP address** | Route policy (ADR 011): private-mode override › per-top-level-site rule › profile default › direct. Proxy (HTTP, SOCKS4/4a/5) and system-VPN routes **fail closed** — never a direct connection. | `routes::tests::precedence_holds_for_every_combination` (all 16 combinations); `network/tests/privacy.rs`: `an_unreachable_proxy_fails_hard_and_never_connects_directly` (HTTP and SOCKS5), `a_proxy_route_goes_through_the_proxy_and_site_rules_pick_it`, `private_mode_overrides_every_other_rule`, `the_system_vpn_route_connects_only_while_the_vpn_is_up`; `browser_core`: `vpn_only_without_a_vpn_fails_closed_rather_than_going_direct` |
| **Host names (DNS)** | DNS over HTTPS with a resolver the user names; none by default; strict (no fallback to system DNS); `localhost` and IP literals never sent (ADR 012). | `dns_over_https_resolves_strictly_and_caches` (a local resolver; `.test` names only DoH can answer); `a_doh_resolver_that_is_not_https_is_refused`; `dns` codec tests and the `dns_message` fuzz target |
| **Cookies** | Third-party cookies blocked by default — not sent, not stored, whatever `SameSite` (P6); `SameSite`, host-only and `Secure` rules (M28) beneath. | `a_samesite_none_secure_cookie_is_not_sent_on_a_third_party_request_by_default`; `two_sites_embedding_the_same_third_party_cannot_link_through_it`; `the_third_party_cookie_setting_reaches_the_network_policy` |
| **Storage** (local storage, HTTP cache) | Keyed by (top-level site, origin); the HTTP cache by top-level site (M28). Granted per page and scoped per document (M36). | `a_third_party_under_two_sites_gets_two_areas`; the cache-key assertion in `two_sites_embedding_…`; `pages_of_different_origins_cannot_observe_each_others_state` (M36, both modes) |
| **Fonts** | The fingerprint policy's font exposure: every granted font, or (private mode) DejaVu, Liberation and Noto only; filtered in the browser process, so a renderer never learns a hidden font's key. | `the_standard_font_set_hides_other_system_fonts_from_a_private_page`; `privacy` unit tests |
| **Time and timezone** (P5) | `Date` and rAF at the policy's resolution (1 ms; 100 ms private) and timezone (real; UTC private; or `privacy.timezone`), through Boa's host hooks — every `Date` method. | `scripts_observe_date_at_the_policys_resolution_and_timezone`, `the_standard_policy_keeps_the_real_timezone_and_millisecond_time`, `animation_frame_timestamps_follow_the_policy`; engine, both modes: `a_private_page_sees_utc_and_coarse_time_in_both_modes` |
| **Screen, GPU info, canvas/WebGL readback** | Not exposed (curated API surface, §6); a size API, if added, reports 100-px buckets (`privacy::bucket_px`, ADR 019 §3). | `no_script_api_exposes_the_screen_or_the_machine`; `sizes_are_bucketed` |
| **Camera, location, clipboard** | Permission policy (M36): origin-scoped, revocable, audited decisions; the capabilities cannot be constructed until their APIs exist. | `browser_core::permissions` tests; `capability` (uninhabited types) |
| **Ads, trackers, AI crawlers** | Filter engine at the network layer, every hop, always on, in the browser process (ADR 019 §1). | `filter` tests incl. `the_golden_corpus`; `the_filter_blocks_before_anything_is_sent_including_redirect_hops`; engine, both modes: `the_filter_blocks_a_pages_subresource_in_the_browser_process`; `filter_list` fuzz target |
| **Identity in headers** | A frozen User-Agent and `Accept-Language`, `Referer` strict-origin-when-cross-origin (hardening pass); a proxy's `CONNECT` line carries the frozen name too (it used to carry `ureq/2.12.1`). | the hardening pass's request-hygiene tests |
| **Trust (P8)** | The bundled Mozilla roots, plus CA files the user adds — never instead of them (ADR 018). | `unusable_files_are_reported_and_never_empty_the_store`; `an_unreadable_extra_ca_file_is_reported_and_the_bundled_roots_remain` |

## What was built

- **`filter` crate** (core, no dependencies but diagnostics): EasyList/ABP
  syntax and hosts-file lines; three categories; token-indexed matching;
  bounded lists, rules and URLs; a built-in third-party tracker list written
  for this project. Requests carry a `Destination` (document, script, image,
  stylesheet, font, xmlhttprequest, other) for the type options.
- **`network`**: `NetworkError::Blocked`; the filter on every hop;
  third-party cookie blocking (`NetworkPolicy::third_party_cookies`); one
  `Transport` for every route; `ProxyRoute`, `SystemVpnRoute`,
  `RoutePolicy` (`RouteRules::choose`); `NetworkConfig` and
  `NetworkContext::with_config`; `dns` (wire codec) and `doh` (resolver,
  cache); `tls` (extra trust anchors). New dependencies: `ureq`'s
  `socks-proxy` feature (the `socks` crate), and direct use of the `rustls`
  and `webpki-roots` versions `ureq` already pulled in.
- **`privacy` crate**: `FingerprintPolicy` (time resolution, timezone, font
  exposure), standard and private presets, `bucket_px`.
- **`js`**: Boa host hooks for time; rAF through the policy;
  `ScriptCapabilities::fingerprint`.
- **`engine`**: `PageParams::fingerprint`; per-page font filtering; the
  policy, destinations and `Blocked` over the renderer protocol.
- **`browser_core`**: settings `privacy.timezone`, `network.third_party_cookies`,
  `network.proxy`, `network.proxy_sites`, `network.private_proxy`,
  `network.vpn_only`, `dns.doh`, `dns.doh_bootstrap`, `tls.extra_roots` —
  all through one validated `Settings::set`; the profile builds its network
  from them and **fails closed** (refuses every request) if it cannot;
  filter lists from `filters/`; `Profile::fingerprint()`;
  `ProfileOptions` (the host's VPN probe, run-only overrides that are never
  saved).
- **`app`**: `--private`, `--proxy`, `--vpn-only` (a Linux probe of
  `tun`/`tap`/`wg`/`ppp` interfaces), `--doh`, `--extra-roots`,
  `--trust-system-store`, `--fetch-filter-list`, and `--set key=value`.
- **Guards:** `filter_list_rejected`, `request_blocked`,
  `route_failed_closed`, `doh_failed` (37 in all).

Found while building, and fixed:
- A command-line override that was *refused* still counted as an override,
  so saving the settings reverted a change the user made to that key in the
  same run (`command_line_overrides_apply_but_are_never_saved`).
- `ureq` accepts `socks5://` with no host; the route now refuses it.
- A test of mine deadlocked by locking the same mutex twice in one
  assertion — recorded because it looked like a filter hang at first.

## Acceptance criteria

- [x] A policy table lists each row of the §6 boundary table, with its
      policy and the test that proves it (above).
- [x] The filter engine blocks known-bad URLs in a golden corpus
      (`the_golden_corpus`: 8 blocked, 8 allowed), stays within a documented
      per-request budget (2 ms in a release test; measured 14–18 µs over
      100,001 rules including a 4 KiB hostile URL), and fuzzes clean
      (`filter_list`).
- [x] Third-party isolation is verified with two sites embedding the same
      third party — cookies, HTTP cache and storage. *Through subresources:
      frames do not exist yet.*
- [x] Route precedence is unit-tested for every override combination.
- [x] An unreachable `ProxyRoute` fails hard and never falls back to a direct
      connection (HTTP and SOCKS5, against a closed port; the server sees
      nothing).
- [x] Scripts observe `Date` at the policy's resolution and timezone (P5),
      in both isolation modes.
- [x] A `SameSite=None; Secure` cookie isn't sent on a third-party request
      under the default policy (P6).

## Verification

Standing Definition of Done: 1,105 tests, 0 failures (up from 1,049);
clippy unchanged at 43; every guardrail and its self-test pass; the fuzz
suite passes (24 targets; new: `filter_list`, `dns_message`; `ipc_decode`
seeds for destinations, the fingerprint policy and blocked replies, pinned
by `the_committed_seeds_of_changed_messages_decode`); `cargo deny`/`cargo
audit` are not installed locally (CI runs them) — the new `socks` and
`byteorder` crates are MIT/Apache-2.0 and Unlicense/MIT, both on the allow
list. AddressSanitizer over `js`, `engine`, `network`, `browser_core`,
`filter` and `privacy` (the new `Date` host hooks included): 0 reports, 1
run. Every new test was checked to fail when what it guards is broken
(e.g. a filter rule that does not match lets the pixel through).

In the real app (Linux, Wayland): `app --private --proxy http://127.0.0.1:1
--set dns.doh=http://dns.example/q --url http://example.com/` reported the
plain-http resolver as refused, and the navigation failed through the proxy
route ("no fallback to a direct connection") — nothing went out directly.
Not done this pass: `--proxy` and `--doh` against real services (Tor, a
public resolver); the local proxy and resolver tests cover those code paths.

## Not done / open

- WireGuard (ADR 011 §5): needs a user-space TCP/IP stack; backlog.
- ~~A live TLS test with a private CA (needs a TLS test server).~~ Closed in
  the gap pass (2026-10-21): `crates/network/tests/private_ca.rs` runs a
  `rustls` server presenting a leaf a private CA issued (committed fixtures,
  valid until 2126; the CA's key was deleted after signing). With the CA as
  an extra root the site is fetched, by host name and by IP; without it the
  connection is refused for its certificate.
- macOS and Windows system trust stores; HSTS and pinning (backlog).
- Per-site route rules are an API (`RouteRules::site_rules`) and one
  setting (`network.proxy_sites` → the proxy); a rule table in the settings
  file is not.
- The filter has no cosmetic (element-hiding) rules, regular-expression
  rules or `$csp`/`$redirect` — skipped and counted, never guessed.
- The VPN probe exists on Linux only; elsewhere `--vpn-only` refuses every
  request (fail closed).
