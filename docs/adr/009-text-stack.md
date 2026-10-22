# ADR 009 — The text stack: parser, shaper, rasterizer, bidi, line breaking, fonts

- **Status:** accepted (Milestone 35, 2026-09-28)
- **Decides:** which libraries parse, shape and draw text; where fonts come
  from and who may read them; how web fonts are admitted; what stays our own
  code.
- **Constrains:** `text`, `layout` (inline layout), `rendering` (glyph runs),
  `style` (text properties), `engine` (web fonts, renderer IPC), `renderer`
  (system fonts), hosts.
- **Supersedes:** Milestone 2's `fontdue`, one bundled font, per-character
  advances.

## Context

Text was Latin-only: one bundled font (DejaVu Sans), `fontdue` for parsing
and rasterizing, advance widths summed per character — no kerning,
ligatures, complex scripts, bidi, fallback, weights or styles. `fontdue` 0.8
never returns from loading a font with one corrupted `cmap` byte
(`fuzz/known-issues/fontdue-cmap-hang.md`), so web fonts could not ship on
it. After Milestone 33, `font-family`, `text-decoration`, `font-weight` and
`line-height` topped the corpus's unsupported-feature report (46–50 of 56
pages each).

## Decision

1. **Parsing: `ttf-parser` 0.25.** Parses lazily (only the table directory up
   front), bounds composite-glyph depth (32), `#![forbid(unsafe_code)]`,
   MIT/Apache-2.0, hosted under the HarfBuzz organisation. It loads, shapes
   and draws the input that hung `fontdue`, promptly (a regression test now).
   RustSec lists it as unmaintained (RUSTSEC-2026-0192). That is accepted, with
   the reasons in `deny.toml`: no `unsafe`, bounded lazy parsing, the
   `font_parse` and `web_font` fuzz targets and the hostile-font tests, and
   web-font bytes validated before parsing. `rustybuzz` depends on it, so
   replacing it means replacing both; that is a risk in `ROADMAP.md` §7.
2. **Shaping: `rustybuzz` 0.20** (a HarfBuzz port; MIT). Kerning, ligatures,
   Arabic joining, Indic reordering, Hebrew — every OpenType shaper, with
   HarfBuzz's operation limits. Its only `unsafe` is `bytemuck` `Pod` marker
   impls. A shaping face costs ~17 µs to build, so it is built once per face:
   font bytes for system and web fonts are kept for the process, shared by
   every page and keyed by file (or content hash), under caps (256 MiB system,
   64 MiB web); past a cap, faces are rebuilt per shaping call (slower, never
   wrong).
3. **Rasterization: our own**, in safe code (`text::raster`): outlines from
   `ttf-parser`, signed-area coverage accumulation, curves flattened into a
   capped number of lines, bitmaps at most 1024 px a side. `ab_glyph_rasterizer`
   was the alternative; it dispatches to SIMD through `unsafe` function
   pointers for a 555-line algorithm we can own.
4. **Bidi: `unicode-bidi` 0.3** (UAX #9; MIT/Apache-2.0; one `unsafe`
   block, in level parsing — the crate Servo uses). **Line breaking:
   `unicode-linebreak` 0.1** (UAX #14; Apache-2.0; no `unsafe`).
   `unicode-script` and `unicode-properties` classify characters.
5. **Web fonts: `wuff` 0.2** (WOFF and WOFF2; MIT; no `unsafe` of its own)
   over `brotli-decompressor` (BSD-3-Clause/MIT; has `unsafe`; Dropbox's,
   widely used). Every web font is sniffed by its signature (never trusted
   by `format()` or content type), bounded before decompression by the size
   its header declares (24 MiB), validated as a font, and fetched as an
   anonymous CORS request — a cross-origin font needs
   `Access-Control-Allow-Origin` for the page. A document's web fonts belong
   to its own font set; no other page sees them.
6. **The engine never enumerates fonts.** A host grants a `text::FontSource`
   (a capability): the catalog content may use — which M37's fingerprinting
   policy will filter — and each font's bytes on request. `renderer::system_fonts`
   asks fontconfig (families, weight, slant, language coverage) or scans the
   font directories. In process mode the renderer gets the catalog in its
   setup and asks the host for a font's bytes over IPC (`FontRequest`); the
   host answers only for keys in the catalog it sent. The renderer's sandbox
   stays closed to the filesystem.
7. **Matching and fallback.** CSS Fonts 4 matching (style, then weight) over
   a family's faces; generic families map to preferred system families; the
   bundled face is always last. Missing weights and styles are synthesized
   (bold widens, oblique slants 14°). Per-character fallback searches the
   bundled face, then the first few catalog fonts covering the character's
   script (by fontconfig language), then web fonts — in that fixed order, so
   parallel layout draws the same thing every time. Face ids are fixed
   (catalog position), not load order, for the same reason.
8. **No subpixel positioning** (unchanged from ADR 017): glyphs are cached per
   (face, glyph, rounded size, synthesis).
9. **Tests and the corpus use a fixed font set** (`test-fonts/`: DejaVu
   subsets, Lohit Devanagari, a Noto Sans SC subset — all freely licensed),
   so goldens do not depend on the machine's fonts.

## Consequences

- `text::Font` is a font *set*; each document gets its own (web fonts are
  per document). Layout measures words by shaping them; words are split at
  UAX #14 opportunities; each line is reordered visually by UAX #9 levels;
  `text-align: start/end` follow the direction.
- Shaping is cached per word; an ASCII word none of whose glyphs any
  substitution or positioning lookup touches is measured from its advances
  without the shaper — exact, and most of the digits and many letters of
  Latin text.
- The first frame of a 10,000-row synthetic page is ~18% slower (every word
  is now shaped or checked); incremental frames are unchanged.
- New dependencies: `rustybuzz`, `unicode-bidi`, `unicode-bidi-mirroring`,
  `unicode-ccc`, `unicode-linebreak`, `unicode-properties`, `unicode-script`,
  `core_maths`, `libm`, `wuff`, `brotli-decompressor`, `alloc-no-stdlib`.
  `cargo deny` passes. `fontdue` is removed.

## Alternatives

- **`swash`** (parser, shaper and scaler in one): fewer crates, but its own
  parser, and a shaper less complete than HarfBuzz for Indic and Arabic.
- **Bindings to HarfBuzz and FreeType** (C/C++): the most complete, and the
  FFI and memory-unsafety this project's principles exclude from the core.
- **Fonts read by the renderer directly** (Landlock granting the font
  directories): simpler, but a compromised renderer could then read every
  font file and enumerate the system's fonts; serving bytes by catalog key
  keeps both with the host.
