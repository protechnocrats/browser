# ADR 010 — Capabilities: authority is a handle, granted by the browser

- **Status:** accepted (Milestone 36, 2026-10-11)
- **Decides:** how authority over the network, storage and every
  permission-gated API is represented, who may create it, how a page gets
  it, and how it is taken back. Also where Browser Core sits and what the
  extension hook points will look like.
- **Constrains:** a new `capability` crate, a new `storage` crate (the
  storage store, moved out of `js`), a new `browser_core` crate, the `js`
  bindings' constructors, `engine::PageParams` and `PageCommand`, the
  renderer setup message (ADR 016), and every Web API added from now on.
- **Fixes:** P4 (`ROADMAP.md` §3): every page's script got `fetch`, XHR and
  `localStorage` automatically — ambient authority.

## Context

Architecture doc §1: *a process, origin, extension, or subsystem gets
exactly the scoped, revocable authority it needs — never ambient
authority.* Until now the `js` crate was handed the profile's
`NetworkContext` and `LocalStorageStore` and wired them into every page's
global object. Nothing in the type system said "this page may use the
network"; nothing could withhold it from one page, or take it back. The file
capability (`images::FileAccess`, hardening pass) and the renderer launcher
(ADR 016) were the only real grants.

Three properties are wanted (`ROADMAP.md` M36):

1. **Unforgeable** — authority is a value of a type, not a string or a flag;
   code that has not been handed one cannot conjure it.
2. **Scoped** — a grant is narrowed to what it is for (storage: one storage
   key, not the profile).
3. **Revocable** — the browser can take a grant back from a live page, and
   every copy of it stops working at once.

## Decision

### 1. Capabilities are object handles

A capability is a value whose possession *is* the authority, in the object-
capability sense. `capability::NetworkCapability` holds the
`NetworkContext`; the only way script code reaches the network is through
one. `StorageCapability` holds the profile's `StorageStore`;
`OriginStorage` holds *one* storage area. Fields are private: outside the
`capability` crate a handle can be cloned, narrowed or revoked, never built.

A page's grants travel together as `capability::PageGrants`
(`network: Option<NetworkCapability>`, `storage: Option<StorageCapability>`).
`PageGrants::none()` grants nothing and needs no authority; anything else is
minted.

### 2. One minting point, the `Authority`, held by the browser

`capability::Authority` mints handles. It is constructed in exactly two
places, enforced by `scripts/check-capabilities.sh` (self-tested):

- `browser_core` — the browser process's profile holds the one authority and
  decides each page's grants;
- `engine/src/renderer.rs` — a renderer process re-creates, *on its side of
  the boundary*, the grants the host listed in the setup message (ADR 016):
  the network capability over its `IpcRoute`, storage over its own store.
  The renderer cannot grant itself more than the host listed: the list is
  the host's, and the host's origin lock still checks every request.

Test code may mint (test modules, `js`'s `test_support.rs` behind the
`test-support` feature, which no production manifest may enable).

Rust cannot make a value unforgeable across crates on its own — a public
constructor is public. The type does the scoping and the revocation; the
guardrail script keeps the constructor where it belongs. That is the same
split as `forbid(unsafe_code)` plus `check-forbid-unsafe.sh`.

### 3. Scoping is attenuation

A broad grant narrows to a specific one; the reverse does not exist.
`StorageCapability::scope(&StorageKey) -> OriginStorage` gives the area of
one storage key. The engine scopes a page's storage grant to each document's
key as the document is installed, and hands `js` only the narrowed handle:
script can reach its own origin's area and no other, because it holds no
handle to any other. `js` checks the handle's key against the document's own
and refuses a mismatch (a `capability_denied` guard) — defence in depth, not
the mechanism.

### 4. Revocation is a shared token

Every handle minted from one grant shares a token. `revoke()` sets it; every
clone, and every handle narrowed from it, fails from then on with
`CapabilityError { kind, denial: Revoked }`. Revoking is always allowed to
whoever holds a handle (giving up authority needs no authority).

The host revokes through the engine API: `PageCommand::Revoke(kind)`. The
page applies it at its next tick, in order with other commands; in process
mode it crosses the wire like any command, and the renderer revokes its
handles. A revoked network capability also rejects the page's fetches still
in flight. Grants are not re-issued to a live page in M36: a new grant is a
new page (or a reload). *(Amended in the gap pass, 2026-10-21: the host
grants again with `PageCommand::Grant(PageGrants)` — `Session::grant(page,
kind)` mints a fresh handle from the profile. Each kind present replaces the
page's grant of that kind, for the document showing now (storage narrowed to
its key) and later ones; the token is new, so a revoked handle stays
revoked. Handles never cross a process boundary: the renderer protocol
carries the kinds (`ToRenderer::Grant`) and the renderer mints its own over
its route and store, as at setup.)*

### 5. Denial is a capability error, not a network error

A page without `Network` (or with it revoked) still has `fetch` and
`XMLHttpRequest` as functions — a page must not crash on a `ReferenceError`
— but `fetch()` rejects with `NotAllowedError: … capability` and `send()`
throws it. Nothing reaches the network or the fetch pool. Without `Storage`,
`localStorage`'s methods throw `SecurityError` (as browsers do with storage
disabled). Each refusal is a `capability_denied` guard event (M27).
`sessionStorage` is the page's own memory, not shared authority, and stays.

### 6. APIs that are not built are types that cannot exist

`ClipboardCapability`, `GeolocationCapability`, `CameraCapability`,
`MicrophoneCapability` and `NotificationsCapability` exist and hold a value
of an uninhabited type. No code can create one, so no binding can be wired
to one, ambiently or otherwise: the compiler checks it. Implementing one of
these APIs means adding a mint method here, a permission decision in
`browser_core`, and a binding that takes the handle — the order this ADR
fixes.

### 7. Permissions are data; the prompt is the shell's

`browser_core::PermissionStore` holds decisions keyed by (origin,
permission): allowed, denied, or not yet asked. A request with no decision
becomes a pending prompt (a `RequestId`) the shell may show; the shell
answers with `resolve`. Decisions are origin-scoped (opaque origins are
always denied and never stored), revocable, persisted under the profile, and
every request, decision and revocation is an `engine::permission` event.
Pending prompts are bounded per origin and in total. Until an API exists, a
granted decision still mints nothing (§6).

### 8. Browser Core

A `browser_core` crate sits above `engine` and below the shells. It is a
host of the engine (it drives the engine API only, checked by
`check-host-boundary.sh`) and holds what is neither engine nor UI:

- `Profile` — a directory (or memory only), the one `Authority`, the
  `NetworkContext`, the `StorageStore`, `Settings` and the
  `PermissionStore`. Decides each new page's `PageGrants`.
- `Session` — the pages (tabs) of one window over one `Engine`, the active
  one, and what each shows; it applies the zoom setting and persists the
  profile as it runs.
- `Settings` — typed, versioned, validated, persisted.

What it decides today is deliberately simple: every web page gets the
network and its own origin's storage — what browsers give — but *because
the profile granted them*, through a path that can withhold or revoke them.
M37's privacy policy hangs off the same place.

## Persistence formats

Settings, permissions and local storage are plain text files under the
profile directory, each starting with a format line naming its version;
written atomically (temporary file, then rename) and owner-only (0600).
Hand-written like every other format here (cookie persistence, ADR 016's
codec): small, bounded and fuzzed (`profile_files`). Reading never trusts
them: a file over its size cap, of an unknown version, or with malformed
lines yields defaults for what could not be read, never a failure to start
and never a value outside its range, and reports `profile_file_rejected`.

Local storage persists only in thread mode in M36: in process mode each
renderer keeps its storage in memory, as before (process isolation's open
item), until M41 serves storage from the browser process.

## Extension hook points (designed, not built)

Architecture doc §7: the engine exposes hook points; the shell builds an
extension system on them. Extensions are principals like pages: each hook
is a capability an extension is granted, minted by the profile's authority,
scoped and revocable — never a global registry anything can append to.

| Hook | Shape | Capability | Notes |
|---|---|---|---|
| Request interception | `trait RequestHook: Send + Sync { fn on_request(&self, req: &RequestView) -> RequestVerdict }` — `Continue`, `Block`, `Redirect(url)` (to http(s) only) | `InterceptRequests { origins: Vec<OriginPattern> }` | Runs in the browser process, in the fetch path beside M37's filter engine, which stays built in (§6.1) — an extension cannot weaken it. Bounded time per call; a slow or panicking hook is skipped and reported. |
| Response observation | `on_response(&self, view: &ResponseView)` — headers and status only | `ObserveResponses { origins }` | No body access in the first version. |
| Content scripts | a script source and match patterns | `InjectScripts { origins }` | Runs in its own `JsEnvironment` in the page's renderer (an isolated world), sharing the DOM, not the global object; the engine API gains a command for it. |
| DOM read | through the content script's DOM | covered by `InjectScripts` | |
| UI panels | the shell's concern | `ShowPanel` | Not an engine hook. |

None of these is built. What M36 commits to is the rule that each will be a
handle from the `Authority`, scoped by origin pattern, revocable through the
same token as every other capability, and audited.

## Consequences

- `JsEnvironment`'s production constructor takes `ScriptCapabilities`; the
  old convenience constructors (a private network, a private store) are test
  support only.
- `engine::PageParams` carries `grants`; `PageParams::new(viewport)` grants
  nothing. A host that wants script to fetch must say so.
- The renderer protocol gains the grant list in `Setup` and the `Revoke`
  command (both in the `ipc_decode` seeds).
- The `js` crate reaches no global mutable state (checked by
  `check-capabilities.sh`): what a script can affect is what its handles and
  its own document reach. Dependencies' process-wide counters (diagnostics
  metrics, node and opaque-origin id allocators) are write-only from a
  page's point of view.
- In process mode the network capability is enforced inside the renderer: a
  compromised renderer can make any request its document loader could (the
  host's origin lock still applies). Enforcing script network access in the
  browser process needs requests the renderer cannot mislabel — M41's
  site-per-process work is where that boundary moves.
