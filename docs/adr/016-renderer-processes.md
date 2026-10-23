# ADR 016 — Pages run in sandboxed renderer processes

- **Status:** accepted (process isolation, 2026-09-26)
- **Decides:** where a page's engine state lives, how it talks to the host,
  where its network requests are made, and what the operating system lets
  it do.
- **Constrains:** `engine` (a second transport behind the same API), a new
  `renderer` host binary, a new `sandbox` platform crate, hosts that create
  engines (`app`), and every message that crosses the process boundary.
- **Amended by:** ADR 025 (Milestone 41): a renderer holds documents of one
  site, a page moves to another renderer when a navigation lands on another
  site, `localStorage` is served by the browser process, and the browser
  enforces the page's network grant (§1, §5 and the first Consequence below).
- **Supersedes:** the "multi-process sandboxing: deliberately not planned"
  line of `ROADMAP.md` §4, and the "thread isolation now … OS-process
  isolation later" note of architecture doc §4.

## Context

Since Milestone 30 every page runs on its own thread (ADR 006), behind a
message-based API whose every type is plain data (ADR 005) — built so that
moving pages into processes "changes the transport, not the API". Two things
make that move urgent rather than eventual:

- **Memory safety ends at our own code.** Every crate forbids `unsafe`, but
  Boa, the image decoders, `fontdue`, `ring` and `wgpu` do not. The
  performance audit found a heap use-after-free in Boa reachable from any
  page (P12, ADR 015). In one process, a page that corrupts memory owns the
  whole browser: every other page, the cookie jar, the GPU thread.
- **Zero-trust (§1) is not enforceable in one address space.** A page thread
  that is compromised can read the profile's cookies and make requests as
  any origin; checks between threads are only as strong as the memory they
  live in.

## Decision

1. **One renderer process per page** (`Isolation::Processes`). The host
   starts the `browser-renderer` binary for each page, and the page's whole
   engine state — DOM, Boa, style, layout, display list, glyph atlas — lives
   there. Per page rather than per site for now: every page is isolated from
   every other and from the host; a cross-site navigation *within* a page
   stays in its process (site-per-process navigation is follow-up work,
   `ROADMAP.md` §4). `Isolation::Threads` (ADR 006) remains, for tests,
   headless tools and platforms without the binary.
2. **The API does not change.** In process mode the host's per-page thread is
   a *bridge*: it relays the page's mailbox (commands, ticks, shutdown) to
   the renderer, and a reader thread publishes what the renderer sends back
   (events, frames, status) into the same mailbox. `Engine::update`,
   `update_blocking`, `next_wake`, the waker, coalescing and crash reporting
   are the ones ADR 006 describes. Inside the renderer, the unchanged
   page-thread loop runs.
3. **The wire is a hand-written, bounded binary codec** (`engine::ipc`),
   length-prefixed frames over the child's stdin/stdout. No `serde`: the
   project writes its formats by hand (cookie persistence, scene JSON), the
   message set is small, and a decoder we wrote is one we can bound and fuzz
   (`ipc_decode` fuzz target). Every length is checked against what remains
   and against a cap; every enum tag is checked; a scene's coordinates must
   be finite. Anything malformed ends the page as crashed — the host never
   guesses.
4. **Frames send what changed.** Display items travel every frame (they are
   only what is on screen), glyph-atlas bands only when their contents
   changed (the band identity of `text::AtlasPixels`), images once per id.
   The host rebuilds a `Scene` identical to the one the renderer drew.
5. **The renderer has no network.** Its `NetworkContext` has an `IpcRoute`:
   every request goes to the host, which makes it with the profile's real
   `NetworkContext` — one cookie jar, cache and (M37) route policy, in the
   process that holds them. The host **checks each request against the
   page**: a navigation with no initiator is allowed only for a URL the host
   itself asked the page to load; any other request must come from an
   origin the page's documents actually have (recorded by the host from the
   navigations it performed). A compromised renderer cannot borrow another
   site's cookies by claiming to be it.
6. **The renderer is sandboxed** (`sandbox::enter`, Linux): after start-up,
   Landlock denies every filesystem access, and a seccomp filter makes
   socket, connect, bind, listen, accept, execve, execveat and ptrace fail
   with `EPERM`. Other platforms run the renderer without a syscall
   sandbox (reported at start-up), still isolated by the process boundary.
7. **Crashes are contained by the operating system.** A renderer that
   panics reports it; one that aborts, segfaults or is killed ends its
   pipe, and the host reports `PageCrashed` with the exit status. Closing a
   page (or dropping the engine) ends its renderer.
8. **Fonts come from the host too** (Milestone 35, ADR 009). The renderer's
   setup carries the host's font catalog; a font's bytes are asked for by
   key (`FontRequest`) and answered only for keys in that catalog. The
   sandbox keeps the filesystem closed.

## Consequences

- `localStorage` is per renderer process until storage moves into the host
  (M36 already puts storage under the profile in Browser Core): two pages of
  one origin no longer share it in process mode. Thread mode keeps the
  shared store. *(Since M41 the browser process serves it: ADR 025 §4.)*
- `EngineConfig` gains `isolation`; `Engine::renderer_pid` exposes a page's
  process for diagnostics.
- New dependencies, in the `sandbox` platform crate only: `landlock` (MIT OR
  Apache-2.0; Landlock LSM bindings, depends on `libc`) and `seccompiler`
  (Apache-2.0 OR BSD-3-Clause; rust-vmm's seccomp-BPF compiler, `libc`).
  Both contain `unsafe` (system calls), both are replaceable behind
  `sandbox::enter`, and neither may appear in a core crate.

## Alternatives

- **Keep threads, add checks.** Checks in the same address space as the code
  they guard do not survive the memory corruption they are meant to contain.
- **`serde` + `bincode`.** Less code, but derives across `rendering` and
  `style` types, a format we do not control, and decoding we would still
  have to bound for hostile input.
- **Process per site now.** Needs the host to move a page between processes
  on cross-site navigation; worth doing on top of this, not instead of it.
