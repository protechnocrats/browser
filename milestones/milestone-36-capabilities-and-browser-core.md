# Milestone 36 — Capability layer & Browser Core seed

> **Status:** Done, with gaps (2026-10-11). **Still open:** local storage
> persists only in thread mode (`--single-process`) — in process mode each
> renderer keeps it in memory, as before → **M41** (storage served by the
> browser process); in process mode the network capability is enforced
> inside the renderer, not by the browser → **M41**; ~~grants are not
> re-issued to a live page~~ (done in the gap pass: `PageCommand::Grant`,
> `Session::grant`, ADR 010 §4 amended); the permissions that
> can be decided (clipboard, location, camera, microphone, notifications)
> mint nothing until their APIs exist → backlog, each with its own
> permission/fingerprint policy first (`ROADMAP.md` §7). Decision record:
> `docs/adr/010-capabilities.md`. Overall status: `AUDIT.md` · the plan:
> `ROADMAP.md`.

**Goal:** make *capability-based* (architecture doc §1) real: authority is
granted, scoped and revocable, never ambient. Add the minimum Browser Core
(profiles, sessions, permissions, settings) later work needs. Fixes P4: every
page's script got `fetch`, XHR and `localStorage` automatically.

## What was built

- **`capability` crate** (core). Authority is a value: `NetworkCapability`
  holds the network context, `StorageCapability` the profile's store, and
  `OriginStorage` one storage area. Private fields; one minting point,
  `Authority`. Every handle of one grant shares a revocation token. Scoping
  is attenuation: `StorageCapability::scope(key) -> OriginStorage`. APIs not
  built yet (clipboard, geolocation, camera, microphone, notifications) have
  capability types holding an uninhabited value — they cannot exist, so
  nothing can be wired to them. `PageGrants` carries a page's grants;
  `PageGrants::none()` needs no authority.
- **`storage` crate** (core): the store moved out of `js` — areas by storage
  key, the 5 MiB quota (now enforced by the area itself), the 512-origin cap,
  and persistence: a versioned text file, written atomically and owner-only,
  read back with a size cap, every line checked and quotas re-applied.
- **`js`**: one production constructor,
  `JsEnvironment::new_with_capabilities(document, page, ScriptCapabilities)`.
  Without a network capability `fetch()` rejects and `XMLHttpRequest.send()`
  throws `NotAllowedError: … network capability` — nothing reaches the network
  or the fetch pool; a revoked one also rejects requests in flight. Without
  storage, `localStorage` throws `SecurityError`; a storage handle for another
  storage key is refused. `sessionStorage` stays the page's own. The old
  convenience constructors mint, so they moved to `test_support.rs`, compiled
  only for tests and the `test-support` feature (dev-dependencies, fuzz).
- **`engine`**: `PageParams { viewport, grants }` (`PageParams::new` grants
  nothing); the page narrows its storage grant to each document's storage key
  as it is installed. `PageCommand::Revoke(kind)` revokes for the document
  showing now and every later one; it is never the command a full inbox
  drops; in thread mode `Engine::send` also flips the shared token at once.
  `Engine::page_grants(page)`. In process mode the setup message lists the
  grants (one bit each) and the renderer re-creates them on its side; `Revoke`
  crosses the wire like any command.
- **`browser_core` crate** (core, a host of the engine):
  - `Profile` — a directory (0700) or memory only: the network context, the
    storage store, settings, permission decisions, and the one `Authority`.
    `grants_for_web_page()` gives each page fresh, independently revocable
    grants: the network, and storage in the profile's store. Opening a
    profile never fails on content: an unreadable file gives defaults and is
    listed in `issues()`.
  - `Session` — tabs over one engine: open, close, switch (the switched-to
    tab's last frame is shown at once), resize every tab, zoom (pages lay out
    at window ÷ zoom; input is scaled back), revoke per page or per origin,
    ask for a permission on behalf of the origin a tab shows, and write the
    profile at most every 5 s — waking an idle window to do it.
  - `Settings` — typed, versioned (`browser-engine settings 1`), validated by
    construction (private fields, checked setters): scroll direction, scroll
    speed (0.1–10), default zoom (0.25–5), privacy mode (stored for M37).
  - `PermissionStore` — decisions as data, keyed by (origin, permission);
    a request with no decision is a pending prompt (`RequestId`) for the
    shell; `resolve`, `dismiss`, `revoke`; opaque origins always denied and
    never stored; 4 pending per origin, 64 in all, 4,096 decisions; every
    request, decision and revocation an `engine::permission` event; persisted.
- **`app`**: drives a `Session`. `--profile <dir>` (default: memory only);
  settings feed the shell's scroll settings and the zoom; Ctrl+T, Ctrl+W,
  Ctrl+Tab, Ctrl+= / − / 0. The shell now reports Control (Command on macOS)
  with each key.
- **`conformance`** replays pages with the grants a profile gives a web page,
  so the corpus still renders what the browser renders.
- **Extension hook points** designed in ADR 010 (request interception,
  response observation, content scripts, panels — each a capability from the
  profile's authority, scoped by origin pattern, revocable); not built.
- **Guards:** `capability_denied`, `profile_file_rejected`,
  `permission_requests_capped` (33 in all).

## What keeps it working

| Property | Test or check |
|---|---|
| A page without Network cannot fetch, and the refusal is a capability error | `js`: `a_page_without_network_cannot_fetch_and_gets_a_capability_error`, `xhr_send_without_network_throws_a_capability_error`; engine, thread and process mode: `a_page_without_network_cannot_fetch_and_a_granted_one_can` (a local server counts zero script requests; the guard counter moves) |
| Revocation is immediate and sticks | `revoking_the_network_refuses_later_and_in_flight_requests`, `revoking_storage_takes_effect_on_the_next_call`; engine, both modes: `a_revoked_network_stays_revoked_for_later_documents`; mailbox: a `Revoke` displaces queued input rather than being dropped |
| No global mutable state in the js bindings; minting only in Browser Core and the renderer; test-support never in a production build | `scripts/check-capabilities.sh` (run by `check-dependency-direction.sh`); 9 self-test cases in `test-guardrails.sh`. On its first run it flagged three places — all test code, which taught it to skip `#[cfg(test)]` items, and one manifest comment |
| Requesting a capability never granted cannot be expressed | The bindings reach the network and storage only through a handle (`Option<NetworkCapability>`, `StorageSource`); the unbuilt capabilities cannot be constructed (uninhabited field) — the compiler checks both |
| Settings round-trip, reject corrupt or out-of-range values, default safely | `browser_core::settings` tests (round trip, setters refuse NaN/∞/out of range, garbage and other versions give defaults, issues bounded); `profile` tests (a damaged profile directory opens with defaults and says why; an oversized file is not read) |
| Two pages of different origins cannot observe each other | Engine, both modes: `pages_of_different_origins_cannot_observe_each_others_state` (local, session and global state; checked to fail when the reader is pointed at the writer's origin); `capability` and `storage` scoping tests |
| Hostile profile files | `profile_files` fuzz target (settings, permissions, local storage: never panics, never out of range, write-back round-trips) with 6 seeds; storage, settings and permission parsers' hostile-file tests |
| The renderer protocol carries grants and revocations | codec round trip; `ipc_decode` seeds `11_setup_with_grants`, `12_revoke_network`, `13_revoke_unknown_kind` |
| Memory safety of the new paths | AddressSanitizer (the CI `sanitizer` command, plus `browser_core`, `capability`, `storage`): 0 reports, 1 run |

## Verification on hardware

Linux 7.2, Wayland. `app --single-process --profile DIR --url` a local page
whose script increments `localStorage.visits` and fetches a file: the
profile directory was created 0700, and the idle window wrote `visits 1` to
`local-storage` (0600) within the flush interval. A restart read it back
and wrote `visits 2`. Before the fix noted below, the same run wrote nothing.
In the default process mode the page ran in `app --renderer` processes (one
for the page, one spare) with no error, and as expected no storage file was
written (renderer storage stays in memory until M41). The tab and zoom
shortcuts were not pressed by a person in this pass.

**Found on hardware and fixed:** an idle window never woke for the 5 s
profile flush, so what a page stored reached disk only at a clean exit.
`Session::next_wake` now includes the next flush while the profile has
unsaved changes (`an_idle_session_wakes_to_write_what_a_page_stored`).

## Measurements

No change to the frame pipeline: a capability check is one atomic load per
`fetch`/XHR/storage call, and the session keeps one `Scene` (shared `Arc`s)
per tab. `frame_bench` was not re-run.

Standing Definition of Done: 1,049 tests, 0 failures (up from 1,001); clippy
unchanged at 43; every guardrail and its self-test pass (plus the new
`check-capabilities.sh`); the fuzz suite passes (22 targets, new:
`profile_files`); ADR 010 written before the code. No new external
dependency.

## Not done / open

- **Local storage in process mode** is per renderer, in memory → M41.
- **Network capability in process mode** is enforced in the renderer; a
  compromised renderer can make any request its document loader could (the
  host's origin lock still applies) → M41.
- ~~**No re-grant to a live page**: grants are fixed at page creation;
  revocation only.~~ Closed in the gap pass (2026-10-21):
  `PageCommand::Grant` hands a live page fresh handles of the kinds it
  carries, its current document's script included, in thread and process
  mode (`a_revoked_grant_can_be_granted_again_to_the_live_page`).
- **Per-origin network/storage policy** (block storage for one site) is not
  expressed: grants are per page. M37's policy decides how.
- `privacy.mode` is stored and read by nothing until M37.
- Zoom scales scenes on the GPU: text above 100% is magnified, not
  re-rasterised at the larger size.
- No tab strip or prompt UI (tab chrome is "deliberately not planned"); tabs
  are keyboard-driven, and pending permission prompts are only data.
