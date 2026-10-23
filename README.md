# browser

An independent web engine written from scratch in Rust. It is not built on
Blink, Gecko or WebKit: the HTML parser, CSS engine, layout, GPU renderer,
networking stack and privacy layer are all our own. The one large borrowed
part is the JavaScript interpreter, [Boa](https://boajs.dev/), which sits
behind an interface so it can be replaced.

> **Status: early and experimental.** The engine renders real pages and
> runs their scripts, but it is not a browser you can use day to day yet.
> Expect missing CSS features, rough edges and breaking changes.

## Design goals

- **Memory-safe.** No `unsafe` code anywhere in the project, and CI
  enforces it.
- **GPU-first.** Everything on screen is drawn through `wgpu`. Scrolling
  and transform animations are compositor updates that skip layout.
- **Thread-safe.** Each page runs on its own thread, and style, image
  decoding and parts of layout run in parallel.
- **Sandboxed.** By default every page renders in its own process, one
  site per process, confined with Landlock and seccomp on Linux.
- **Capability-based.** A page holds network, storage and file access only
  as scoped, revocable handles granted by the browser, never as ambient
  authority.
- **Privacy-first.** Partitioned storage and cache, third-party isolation,
  a built-in content filter, DNS over HTTPS with no default vendor, and
  fingerprinting defenses for timers, `Date` and fonts.
- **Vendor-independent.** No built-in accounts, telemetry or default
  services.

## What works today

- HTML parsing and a full DOM, with DOM mutation from script.
- CSS selectors, the cascade, custom properties, `@media`, transitions and
  `@keyframes` animations.
- Block, inline, flex, grid and table layout, floats and positioning, with
  incremental relayout that costs only what changed.
- Text shaping, bidirectional text, line breaking, font fallback and web
  fonts.
- PNG, JPEG, GIF and WebP images.
- HTTP(S) with a cache, cookies, `fetch`, `XMLHttpRequest`, and web
  storage.
- JavaScript, with DOM bindings generated from WebIDL.
- Events, forms with validation, `contenteditable`, links and navigation,
  tabs and zoom.
- An accessibility tree computed from the DOM.

The engine is tested against a corpus of 56 real websites, recorded
offline: all 56 render without crashes or timeouts. Some well-known gaps
remain, including list markers, gradients, `@supports`, `:has()`, text
selection and an OS screen-reader bridge.
[`ROADMAP.md`](ROADMAP.md) has the full list and what comes next.

## Getting started

You need a current stable [Rust toolchain](https://rustup.rs/). On Linux the
window also needs the usual `winit`/`wgpu` system libraries: X11 or
Wayland, plus Vulkan or OpenGL drivers.

```sh
git clone https://github.com/protechnocrats/browser.git
cd browser
cargo run -p app --release                              # open the demo page
cargo run -p app --release -- --url https://example.com/
```

In the window, <kbd>Ctrl</kbd>+<kbd>T</kbd> opens a tab,
<kbd>Ctrl</kbd>+<kbd>W</kbd> closes it, <kbd>Ctrl</kbd>+<kbd>Tab</kbd>
switches tabs, and <kbd>Ctrl</kbd>+<kbd>=</kbd> / <kbd>-</kbd> /
<kbd>0</kbd> zoom.

Some useful flags:

| Flag | What it does |
|---|---|
| `--url <URL>` | Open this page instead of the demo page. |
| `--profile <DIR>` | Keep settings, storage, cookies and permissions in `DIR` between runs. Without it, nothing is kept. |
| `--private` | Private browsing. |
| `--proxy socks5://HOST:PORT` | Route all traffic through a proxy (e.g. Tor on `127.0.0.1:9050`). |
| `--doh <URL>` | Resolve names with DNS over HTTPS through the given server. |
| `--trace` | Print per-frame stage timings to stderr. |

The full list is at the top of [`app/src/main.rs`](app/src/main.rs).

### Without a window

```sh
cargo run -p engine --release --example engine_headless -- page.html   # first frame as JSON
cargo run -p engine --release --example frame_bench                     # frame-cost benchmark
```

## Building and testing

```sh
cargo test --workspace --release                                   # the whole suite
cargo test --manifest-path fuzz/Cargo.toml --no-default-features   # fuzz replay, stress and mutation (stable Rust)
```

Only `gpu_backend`, `shell_desktop` and `app` need a display or a GPU; every
other crate builds and tests headless.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) also runs
these checks, and each one can be run locally:

| Script | What it checks |
|---|---|
| `scripts/check-forbid-unsafe.sh` | Every crate forbids `unsafe`. |
| `scripts/check-dependency-direction.sh` | Core crates never depend on `wgpu`, `winit` or a tracing subscriber, and hosts use only the `engine` API. |
| `scripts/clippy-ratchet.sh` | The number of clippy warnings never grows. |
| `scripts/check-advisory-exceptions.sh` | Every `cargo audit` exception has an expiry date that hasn't passed. |
| `scripts/check-psl-freshness.sh` | The vendored Public Suffix List is up to date. |

Every crate under `crates/` must be listed in `scripts/core-crates.txt` or
`scripts/platform-crates.txt`.

## How the project is organised

| Path | Contents |
|---|---|
| `crates/` | The engine, one crate per subsystem: `html_parser`, `dom`, `css_parser`, `style`, `layout`, `text`, `rendering`, `gpu_backend`, `network`, `script`, `js`, `privacy`, `renderer` (the sandboxed page process), `engine` (the public API) and more. |
| `app/` | The desktop browser built on `engine`. |
| `fuzz/` | Fuzz targets and their corpus. |
| `browser-engine-architecture.md` | The architecture and the principles behind it. |
| `docs/adr/` | Architecture decision records: why each major design choice was made. |
| `ROADMAP.md` | What's done, what's next, and the backlog. |
| `AUDIT.md` | The verified current state: test counts, each principle checked against the code, and every open gap. |
| `milestones/` | One write-up per completed milestone. |
| `test-fonts/` | A fixed set of freely licensed fonts, so test results don't depend on the fonts installed on your machine. |

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

Copyright (C) 2026 Protechnocrats

This program is free software: you can redistribute it and/or modify it
under the terms of the GNU General Public License version 3, as published
by the Free Software Foundation (`SPDX-License-Identifier: GPL-3.0-only`).

This program is distributed in the hope that it will be useful, but
WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
or FITNESS FOR A PARTICULAR PURPOSE. See [`COPYING`](COPYING) for the full
text. Bundled third-party fonts and
data keep their own licenses; see the license files next to them.
