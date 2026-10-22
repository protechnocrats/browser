# ADR 019 — Privacy policies: filtering, third parties, fingerprinting

- **Status:** accepted (Milestone 37, 2026-10-11)
- **Decides:** the request filter (format, where it runs, what ships), the
  third-party cookie and storage rules, and the fingerprint policy that
  script-visible values go through (including `Date`, P5).
- **Constrains:** a new `filter` crate, `network`, a new `privacy` crate,
  `js`, `engine`, `browser_core`.

## 1. The filter engine

- **One engine, three list categories** (architecture doc §6.1): ads,
  trackers and AI crawlers, in Adblock Plus / EasyList syntax plus hosts-
  file lines (`0.0.0.0 example.com`). The subset understood: `||domain^`
  anchors, `|` start/end anchors, `*` and `^`, `@@` exceptions, and the
  options `third-party`/`~third-party`, `domain=`, and the request types
  `script`, `image`, `stylesheet`, `font`, `xmlhttprequest`, `document`,
  `other`. Element-hiding rules (`##`) and options this engine cannot
  evaluate (`csp=`, `redirect=`, …) are skipped and counted, never guessed.
- **At the network layer, always on, in the browser process**: every hop
  of every request (redirects included) is checked before it is sent; a
  blocked request fails with `NetworkError::Blocked`. Not an extension's
  job (§7). A top-level navigation is blocked only by a `$document` rule.
- **Bounded:** lists over 16 MiB, rules over 1 KiB and more than 300,000
  rules are refused; matching is linear in the URL and the candidate rules,
  which are found through a token index (no rule scan per request). The
  per-request budget is measured and tested (milestone file). The parser
  is fuzzed (`filter_list`): lists are untrusted input.
- **What ships:** no third-party list is bundled. A bundled list would go
  stale: lists change daily, and a copy frozen into a release blocks
  yesterday's trackers. The licence is no longer an obstacle: EasyList and
  EasyPrivacy are GPLv3-or-later / CC BY-SA 3.0, compatible with the
  project's GPL-3.0-only (bundling one would keep its own licence and
  attribution beside it). The profile loads list files
  from its `filters/` directory (`ads-*.txt`, `trackers-*.txt`,
  `ai-*.txt`); `app --fetch-filter-list URL` downloads one when the user
  asks. A small built-in list written for this project blocks a handful of
  well-known third-party analytics and advertising hosts (host names are
  facts, not anyone's list content), so the engine is never empty; the AI
  category ships empty — its lists are the user's.

## 2. Third parties

- A request is **third-party** when its site differs from the top-level
  site (the page's site; for a navigation, its own).
- **Third-party cookies are blocked by default** (`NetworkPolicy::
  third_party_cookies = Block`): no `Cookie` header and no stored
  `Set-Cookie` on a third-party request — including `SameSite=None;
  Secure` cookies (P6). Navigations are first-party by definition.
- **Storage is keyed by (top-level site, origin)** (`StorageKey`, M28):
  the same third party embedded under two sites gets two areas. The HTTP
  cache was already partitioned by top-level site.

## 3. The fingerprint policy

`privacy::FingerprintPolicy` is consulted wherever script can observe the
machine. Fields and defaults:

| Field | Default | Private mode |
|---|---|---|
| `time_resolution_ms` — `Date.now()`, `new Date()`, rAF timestamps | 1 | 100 |
| `timezone` — what `Date` reports as local time | the real one | UTC |
| `fonts` — which host fonts content may use | all | the standard set (DejaVu, Liberation, Noto) |

- `Date` goes through Boa's host hooks (current time, local offset), so
  every `Date` method — not just the ones a shim would patch — sees the
  policy.
- Screen and viewport sizes: no script API exposes them (`screen`,
  `innerWidth`, `devicePixelRatio`, `navigator` do not exist); a test keeps
  it that way. If one is added it must report bucketed values (to the
  nearest 100 px) — this ADR is that requirement. CSS media queries see the
  real viewport, which layout needs.
- Canvas, WebGL and audio readback do not exist (architecture doc §6's
  curated API surface).
- `browser_core` picks the default or the private policy from the
  `privacy.mode` setting; `timezone` can be set on its own
  (`privacy.timezone = auto | real | utc`; `auto` follows the mode).
- The policy is per page (`PageParams::fingerprint`), so a private tab and a
  normal one can share an engine; in process mode it travels in the
  renderer's setup message.
