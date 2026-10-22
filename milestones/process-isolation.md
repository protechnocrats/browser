# Process isolation — every page in a sandboxed renderer process

> **Status:** Done (2026-09-27). **Still open:** a process per *site*, storage
> served by the browser process, and sandboxes beyond Linux → **M41**; a GPU
> process → `ROADMAP.md` §4. Decision record: `docs/adr/016-renderer-processes.md`.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** architecture review finding A1 (`ROADMAP.md` §3): every page ran in
the browser's own process, so memory corruption in a dependency — as found in
Boa (P12, ADR 015) — owned the whole browser, and zero-trust checks shared the
address space of what they guarded. Move each page into an operating-system
process of its own, sandboxed, without changing the engine API.

## What was built

- **`Isolation::Processes`** (`EngineConfig::isolation`). Each page gets a
  renderer process. On the host, the page keeps the same mailbox as a page
  thread (ADR 006); a *bridge* thread relays its commands, ticks and shutdown
  to the renderer, and a *reader* thread publishes what comes back. Inside the
  renderer (`engine::run_renderer`), the unchanged page loop runs. `update`,
  `update_blocking`, `next_wake`, the waker, input coalescing and crash
  reporting behave as before. `Isolation::Threads` remains.
- **A launcher capability.** The engine never starts a process:
  `RendererLauncher` is granted by the host. `renderer::ProcessLauncher`
  starts a binary with `--renderer` and an **empty environment**; `app`
  launches itself (one executable, every process kind), and the
  `browser-renderer` binary serves tests and other hosts.
  `engine::InProcessLauncher` runs the real protocol over OS pipes on a thread
  (tests, benchmarks).
- **The wire** (`engine::ipc`): length-prefixed frames, written and read by
  hand. The decoder checks every length against the rest of the message and a
  cap before allocating, every tag, and that geometry is finite; a malformed
  message ends the page as crashed (`Guard::RendererMessageRejected`). A frame
  carries the display items, only the glyph-atlas bands that changed (by
  identity — the atlas's copy-on-write bands), and image pixels once per id;
  the host rebuilds a `Scene` identical to the renderer's.
- **The network stays in the browser.** A renderer's `NetworkContext` has an
  `IpcRoute`: every request goes to the host, which makes it with the
  profile's `NetworkContext` (cookie jar, cache, M37's route policy) on the
  bounded fetch pool. The host's **origin lock** (`SiteLock`) believes only
  what it has seen: a request with no initiator only for a URL the host told
  the page to load; any other only from an origin a navigation it performed
  landed on (opaque origins pass — they are cross-site to everything). A
  refusal is `Guard::RendererRequestRefused` and never reaches the network.
- **The sandbox** (`sandbox::enter`, a new platform crate). On Linux, after
  start-up work: Landlock handles every filesystem right and grants none;
  a seccomp filter on every thread makes `socket`, `socketpair`, `connect`,
  `bind`, `listen`, `accept(4)`, `execve(at)`, `ptrace` and
  `process_vm_readv/writev` fail with `EPERM`. Other platforms report that
  they run with process isolation only.
- **A spare renderer.** A renderer parses its font (17–25 ms) before waiting
  for its setup; the engine keeps one started ahead of need, so a new page
  does not wait for it.
- **Crashes.** A panic in a renderer is reported by it; an abort, a segfault
  or a kill ends its pipe, and the host reports `PageCrashed` with the exit
  status. Closing a page, or dropping the engine, ends its process.

Found and fixed on the way:
- `console.log` printed to standard output — a renderer's channel to the
  browser. It now goes to standard error.
- **A new page never drew its blank first frame** (thread mode too): the
  first `update` replaced the page's "due at creation" with the empty status
  of a page that had not ticked yet. Pinned by
  `a_new_page_draws_its_blank_first_frame_unprompted`.

## What keeps it working

| Property | Test or check |
|---|---|
| A page draws exactly the same in a process as on a thread | `process_isolation.rs`: scene JSON equal after load and three scrolls (fixed box, opacity, shadows, script-built content) |
| The network is reached only through the host | the cookie lands in the host's jar; a real sandboxed renderer (no sockets) loads a page with its stylesheet and image |
| A forged origin or typed navigation is refused before the network | `SiteLock` unit test; end to end with a scripted fake renderer: refused, and the server sees nothing |
| A hostile renderer cannot hurt the host | garbage, a 4 GiB length claim, silence: page crashed, no frame drawn; codec tests reject every truncation, bad tags, non-finite coordinates, oversized images, out-of-range atlas bands; `ipc_decode` fuzz target with 9 real-message seeds and 2 stress inputs |
| Crashes are contained | a panicking renderer; a renderer killed with `SIGKILL` (reported with its signal) while another page carries on |
| Processes end with their pages | closing a page ends its process, not the other's; dropping the engine ends the rest |
| The sandbox holds | `browser-renderer --sandbox-self-test`: reading and writing files, opening sockets and starting programs all refused (exit 0) |
| The engine cannot start processes or read the environment | `scripts/check-core-capabilities.sh` (self-tested) |
| Landlock and seccomp stay in `sandbox` | `deny.toml` bans with `wrappers = ["sandbox"]` |
| Memory safety of the new code | the CI `sanitizer` job runs the engine suites (which include the in-process protocol tests) under AddressSanitizer: 0 reports in 3 runs |

## Measurements

`frame_bench` (median frame; `--processes` uses the in-process launcher, so it
measures the wire — encoding, pipes, decoding — not process start-up), two
rounds each:

| Page | Frame | Threads | Processes |
|---|---|---|---|
| demo | first | 3.4 ms | 14–16 ms (a spare renderer still parsing its font) |
| 10k rows | first | 48–50 ms | 37–44 ms |
| | scroll | 0.04–0.09 ms | 0.33–0.35 ms |
| | one DOM change | 0.46–0.65 ms | 0.69–0.85 ms |
| | caret/focus | 0.02–0.04 ms | 0.15–0.19 ms |

About 0.1–0.3 ms per frame for crossing the boundary. Before the spare
renderer, every first frame paid about 30 ms.

On hardware (AMD Radeon 780M, Wayland, Linux 7.2), `app --url` with the
default isolation: the page ran in a child `app --renderer` process that
reported `filesystem: closed (Landlock); system calls: sockets, exec and ptrace
denied (seccomp)`; `/proc` showed a seccomp filter, `NoNewPrivs`, no sockets
and an empty environment in the renderer, while the page's fetches were logged
by the browser process; frames reached the GPU.

Standing Definition of Done: 943 tests, 0 failures (up from 924); clippy
unchanged at 43; every guardrail and its self-test pass (plus the new
`check-core-capabilities.sh`); the fuzz suite passes (20 targets). New
dependencies (`landlock`, `seccompiler`, `libc`, in `sandbox` only) are
recorded in ADR 016; `cargo audit`/`cargo deny` are not installed locally and
run in CI.

## Not done / open

- **Process per page, not per site**: a cross-site navigation stays in its
  page's process; the origin lock keeps up to four origins → M41.
- **`localStorage` is per renderer process** in process mode (thread mode
  still shares it per origin) → M41 serves storage from the browser process.
- **No system sandbox on macOS or Windows** yet (process isolation only) → M41.
- The GPU thread still lives in the browser process (a GPU process is backlog).
- The renderer's font is parsed per process (about 20 ms, hidden by the spare
  renderer but paid in CPU). *(Measured with `fontdue`; M35 replaced the text
  stack with lazy `ttf-parser` loading, and this figure was not re-measured.)*
