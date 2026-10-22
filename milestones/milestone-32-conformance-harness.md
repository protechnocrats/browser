# Milestone 32 — Real-site conformance harness & fuzz expansion

> **Status:** Done, with gaps. **Still open:** ~~the `fontdue` `cmap` hang blocks web fonts → M35~~ — resolved in M35 (`fontdue` replaced, ADR 009). libFuzzer runs are unexecuted locally; OSS-Fuzz deferred (reason below).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** find the bugs synthetic tests cannot predict. Before this, `--url`
had never been pointed at a real website in a test, and the fuzzers covered
no font, header or script input.

**Builds on:** Milestone 25 (fuzzing), 27 (diagnostics), 29 (the headless
engine API).

## What was built

- **`crates/conformance`**, a host over the engine API:
  - `RecordingRoute` / `ReplayRoute` — `NetworkRoute` implementations that
    record every response a live load fetches (through the engine's own
    `DirectRoute`, so the same requests) and serve them back, offline.
  - Snapshots (`conformance/snapshots/<name>/`: a text `manifest` plus
    deflate-compressed bodies) — the architecture doc §9 snapshot of URL,
    viewport and every response. Size budget: documents/CSS/JS in full,
    images to 384 KiB per page (1.5 MiB for image-heavy pages).
  - A runner that replays a snapshot on a fixed clock, classifies it
    (rendered / load failed / crashed / timed out), loads it twice to check
    determinism, and collects the unsupported features it used.
  - `conformance record | run [--bless] | resave` and a ranked report.
- **56 recorded pages** (`conformance/pages.txt`): encyclopedia, news,
  forums, forms, documentation, CSS-heavy landing pages, image-heavy pages,
  large tables, right-to-left (Arabic, Hebrew), CJK, Devanagari, scripted
  pages. 15 MB. Each with its source and licence.
- **Golden scenes** for every deterministic page (all 56), checked by
  `cargo test` (`crates/conformance/tests/corpus.rs`), regenerated with
  `--bless`.
- **A CPU rasteriser** (`rendering::raster`) so a headless run produces PNGs:
  rects with rounded corners, atlas glyphs, images, sRGB source-over — the
  GPU backend's rules. Bounded to 8192 px a side.
- **Unsupported-feature counters** (`diagnostics::unsupported`): unknown CSS
  properties and `display` values, unknown at-rules and pseudo-classes, script
  errors, non-UTF-8 charsets. Off by default (one atomic load), bounded,
  local-only.
- **Five new fuzz targets** (15 total): `font_parse` (TTF/OTF via `fontdue`),
  `font_face_src` (new `@font-face` `src` parser), `content_decoding`
  (gzip/deflate bodies), `http_headers` (cache headers, `Content-Type`),
  `js_script` (arbitrary script against a fixed DOM, then click/keys/rAF/
  fetch — with a network route that refuses everything). Plus a whole-font
  corruption test over the complete bundled font.
- **CI**: the corpus replay is part of `cargo test`; a CI step publishes the
  report and PNGs as an artifact; the nightly libFuzzer job discovers the new
  targets from `cargo fuzz list`.

## What the corpus found (all fixed, each with a regression test)

| Bug | Found on | Fix |
|---|---|---|
| Documents without `<html>` were rejected; without `<body>` were not pages | `rfc-2119` (bare `<pre>`), `openbsd-home` (omits `<head>`/`<body>`) | The parser implies `<html>`, `<head>`, `<body>` as the HTML standard does |
| A gzip body sent without being asked for was parsed as HTML | `python-home` (CDN) | Bounded `Content-Encoding` decoding (gzip/deflate), `Accept-Encoding: gzip, deflate`; compression bombs stop at the 100 MiB cap |
| `@charset`/`@import`/`@namespace` swallowed the following rule | many stylesheets (spotted in code, confirmed by a test that fails on the old parser) | Statement at-rules end at `;` |
| Every CSS `url()` was fetched and decoded as an image — web fonts, cursors, masks | 6 MB of fonts in the first recording | Only `background`/`background-image` URLs are fetched |

**And one that is not fixed, because it is not ours:** one flipped byte in a
font's `cmap` table makes `fontdue` 0.8 never return from loading
(`fuzz/known-issues/fontdue-cmap-hang.md`, ignored regression test in
`text`). No web content can reach it today (no web fonts), and it **blocks
Milestone 35**: web fonts must not ship on a parser with a known unbounded-time
input; ADR 009 must choose one that survives it.

Also learned: Stack Exchange answers the engine's frozen user agent with 403
(bot protection) — replaced with other forum sources; Wikimedia rate-limits
bursts with a 200 "Wikimedia Error" page — the affected pages were
re-recorded one at a time.

## The ranked report — input to M33 and M35

Top of `conformance run` (pages using the feature, of 56):

| pages | feature | milestone |
|---|---|---|
| 48 | `text-decoration` | M35 |
| 47 | `font-weight`, `font-family` | M35 |
| 47 | `max-width` | M33 |
| 46 | `line-height` | M35 |
| 44 | `position` (41 `top`, 37 `left`, 37 `z-index`) | M33 |
| 43 | `background` shorthand | M33 |
| 42 | `display: none` | M33 |
| 42 | `:root` | M33 (selectors) |
| 40 | `overflow` | M33 |
| 38 | `min-width` (33 `min-height`, 27 `max-height`) | M33 |
| 37 | `display: inline-block` | M33 |
| 36–37 | legacy `:before`/`:after` (single colon) | M33 (generated content) |
| 33 | `float` (29 `clear`), `box-shadow` | M33 |
| 26 | JS: `window is not defined` | unscheduled (JS API breadth) |

These set M33's and M35's priority
order.

## Acceptance criteria

- [x] ≥ 50 snapshots recorded: **56**; the harness runs in CI (as part of
      `cargo test`, ~6 s for the corpus).
- [x] Zero panics/timeouts across the set; every failure found has a
      regression test (table above).
- [x] The report ranks unsupported features by frequency; the top 10 feed
      the M33/M35 backlogs.
- [x] Every listed fuzz target exists and is in the nightly job:
      TTF/OTF font parsing, `@font-face` `src`, URL parsing (`url_origin`,
      `origin_parse`), `Content-Type` and header parsing (`http_headers`,
      `content_decoding`), the cookie parser (`cookie_header`), the JS→DOM
      binding surface (`js_script`). Deep local run: 144,000 mutations across
      15 targets clean; whole-font corruption found the `fontdue` hang.

## OSS-Fuzz

Evaluated, **deferred**. OSS-Fuzz needs a public repository and runs on
Google's infrastructure with results reported there first; this repository
has no public remote yet, and making continuous fuzzing depend on one
vendor's service sits badly with §1 (vendor-independent). The nightly
libFuzzer job (`.github/workflows/fuzz-nightly.yml`) is the self-hosted
equivalent. Revisit when the repository is public — as an *addition* to the
nightly job, never a replacement.

## Not done / open

- The libFuzzer runs themselves still have not executed on this machine (no
  `g++`); the stable replay/stress/mutation suite ran instead.
- Rendering is compared as scene JSON, not pixels; the PNGs are for people.
- Recorded pages are third-party content, so the recordings stay out of the
  repository.

Standing Definition of Done: 773 tests, 0 failures (up from 753); clippy
unchanged at 43; all guardrails pass (`conformance` added as a checked host).
