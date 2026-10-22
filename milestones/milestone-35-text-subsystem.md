# Milestone 35 — Text subsystem

> **Status:** Done, with gaps (2026-09-28). **Still open:** the script
> snapshots await a person's sign-off before they count as frozen; the
> corpus has no recorded web fonts (it needs a re-record); `letter-spacing`,
> `word-spacing`, `overflow-wrap`/`word-break`, `text-overflow`,
> `font-variant`/`font-feature-settings` and dictionary line breaking (Thai)
> → backlog. Decision record: `docs/adr/009-text-stack.md`. Overall status:
> `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** text as its own subsystem. It was Latin-only, with one bundled font
and no shaping, bidi or fallback; its parser (`fontdue`) hung on one corrupted
byte, which blocked web fonts.

## What was built

- **A new text stack** (ADR 009): `ttf-parser` parses (lazily, bounded),
  `rustybuzz` shapes (kerning, ligatures, Arabic joining, Indic reordering,
  every OpenType shaper), our own safe rasterizer draws, `unicode-bidi` and
  `unicode-linebreak` order and break lines. `fontdue` is gone; the input
  that hung it is a passing regression test and a fuzz seed.
- **`text::Font` is a font set**: the bundled DejaVu Sans plus the fonts the
  host grants (`FontSource` — the engine never enumerates fonts), matched per
  CSS Fonts 4 (style, then weight), generic families, per-character fallback
  by script, synthesized bold and oblique, a shaping cache per word, and an
  exact fast path for words no OpenType lookup touches. Face ids and
  fallback order are fixed, so parallel layout draws the same every time.
- **Fonts in renderer processes come from the host**: the catalog in the
  setup message, bytes by key over IPC — the sandbox stays closed.
  `renderer::system_fonts` asks fontconfig or scans the font directories.
- **CSS:** `font-family`, `font-weight` (incl. `bolder`/`lighter`),
  `font-style`, the `font` shorthand, `line-height` (normal, number, length,
  percentage, with half-leading), `text-decoration` (lines, colour; they
  propagate to descendants and cannot be removed by them), `text-transform`,
  `direction` (and the `dir` attribute), `unicode-bidi` (parsed),
  `text-align: start | end` (the new initial value). The user-agent sheet
  bolds headings, italicises `em`/`i`, uses monospace for `code`/`pre`,
  underlines links and strikes `s`/`del`. All text properties share one
  inherited, copy-on-write `Arc` per element (`Style` stays under its
  512-byte bound).
- **Layout:** words are shaped per their style and split at UAX #14
  opportunities (CJK wraps between characters, a line may end after a
  hyphen); each run's bidi levels are resolved as one paragraph and each
  line is reordered visually (rule L2); right-to-left words are shaped right
  to left; start/end alignment follows the direction.
- **Paint:** glyph runs are drawn from the shaped glyphs (the atlas is keyed
  by face, glyph, size and synthesis); underline, overline and line-through
  are drawn from the font's own metrics.
- **Web fonts:** `@font-face` rules fetch their first readable source (TTF,
  OTF, WOFF, WOFF2) as an anonymous CORS request; the bytes are sniffed,
  bounded before decompression, decoded off the page thread and validated;
  the font joins the document's own font set (a new document gets a new
  set). Before the first frame when images are waited for too; otherwise the
  text is laid out again when the font arrives.
- **Fixed test fonts** (`test-fonts/`): DejaVu subsets, Lohit Devanagari and a
  Noto Sans SC subset, used by tests and the conformance corpus so goldens do
  not depend on the machine.

Found and fixed on the way:
- A stylesheet's `url()`s were resolved against the document, not the
  stylesheet — `../fonts/x.woff` next to `/css/site.css` was never found
  (`background-image` URLs too).
- A lock-order inversion between matching and loading deadlocked pages whose
  layout ran in parallel (the corpus timed out). Regression test with a
  watchdog; the test fails on the old order.
- Universal `[dir]` rules in the user-agent sheet made every element pay a
  check (and doubled layout time); `dir` is a presentational hint instead.
- A tracing test built two frames with two clocks (and a transition between
  them): one clock now.

## Acceptance criteria

| Criterion | Evidence |
|---|---|
| Arabic, Hebrew, Devanagari and CJK snapshots render in the correct order with correct shaping; a human verifies them once, then they are frozen | The corpus's `wikipedia-ar/he/hi/ja/zh-browser` pages, re-blessed after inspection: Arabic joined and right to left, Hebrew right to left, Devanagari conjuncts and pre-base vowel signs, Chinese and Japanese wrapping between characters. Unit tests pin the mechanics: `arabic_is_joined`, `right_to_left_runs_come_out_in_visual_order`, the Devanagari reordering in `real_bold_and_fallback_come_from_the_directory`, `a_right_to_left_paragraph_reads_from_the_right`, `a_hebrew_phrase_inside_english_is_reversed_as_a_run`, `cjk_text_wraps_between_characters`. **🚫 The one-time human sign-off is still owed** (the inspection was done by the agent). |
| Malformed and oversized font fuzzing runs clean, including the `fontdue` hang input | `font_parse` and the new `web_font` target (WOFF, WOFF2, TTF seeds, the hang input, the hostile fonts) replay clean; the hang input loads, shapes and draws in the regression test. |
| A hostile web font (huge glyph count, recursive composite glyphs) is rejected within a bounded time and memory budget | `hostile_fonts.rs`: 65,000 glyphs of 30 em squares, and composites that include themselves or loop — loaded, shaped and drawn in 46 ms (release), bitmaps capped at 1024 px, the atlas at its cap; a 65,535-glyph font is refused outright; WOFF containers claiming a huge output are refused before inflating. |
| The `font-*`, `text-decoration` and `line-height` rows leave the top of the corpus report | Before: `font-family` 50 pages, `text-decoration` 49, `font-weight` 49, `line-height` 46, `font-style` 37, `font` 23 — the top of the report. After: none of them in the report. |

## Verification

**On hardware**: `app --url` a page with bold, italic, underline, strikeout,
monospace, a web font, Arabic, Hebrew, Devanagari, Greek and Cyrillic, in
process mode with the system's fonts: no error, no guard report. The same
page rendered headlessly through real sandboxed renderer processes (system
fonts served by the host over IPC) drew every one of them correctly — real
bold and oblique faces, the web font, right-to-left order and alignment,
Devanagari shaping. CJK drew boxes: this machine has no CJK font (the test
fonts cover it for the corpus).

**Process isolation**: the thread-versus-process scene test now includes bold,
italic, Devanagari, CJK and Hebrew from a font source: identical scenes.

**Determinism**: corpus goldens blessed from a release build match a debug
build, twice.

## Measurements

`frame_bench`, interleaved against the commit before M34 (median frame):

| Page | Frame | Before | M35 |
|---|---|---|---|
| demo | first | 3.1–4.2 ms | 4.1–4.8 ms |
| 1k rows | first | 6.4–7.8 ms | 7.8–9.3 ms |
| 10k rows | first | 45.0–49.5 ms | 54.7–57.6 ms (+18%) |
| | scroll / DOM change / caret | 0.03–0.09 / 0.45–0.62 / 0.02–0.04 ms | 0.03–0.07 / 0.47–0.57 / 0.02–0.04 ms |
| 10k + flex/vars | first | 51.2–53.6 ms | 63.1–65.2 ms |

Incremental frames are unchanged. The first-frame cost is shaping (every
word) and bigger boxes: `LayoutBox` 144 → 160 bytes (a word carries its
shaping spec and decoration). A first version was 60% slower; it was brought
down by a word-level shaping cache, an exact fast path for words no lookup
touches (with per-face ASCII glyph and advance tables), measuring widths
without building runs, and replacing SipHash in the caches — whose FNV
replacement first needed a bit mixer (short keys all landed in few buckets).
A profiler (`samply`) needs `perf_event_paranoid` ≤ 1, which was not changed
on this machine; the remainder is recorded, not explained further.

Standing Definition of Done: 1,001 tests, 0 failures; clippy unchanged at
43; every guardrail passes; the fuzz replay passes (21 targets, new:
`web_font`); `cargo deny check licenses bans` passes; AddressSanitizer: 0
reports over the `js`, `engine`, `text` and `gpu_backend` suites.

## Not done / open

- **Human sign-off of the script snapshots** (the criterion's wording).
- **Web fonts in the corpus**: it was recorded before web fonts were fetched;
  a re-record will bring them (and the background images its old URL bug
  requested wrongly).
- `unicode-range` is ignored (every subset loads); `local()` sources are
  skipped; `font-display` is "block" before the first frame and "swap"
  otherwise.
- `unicode-bidi: embed/isolate/bidi-override` are parsed but only the
  paragraph direction is applied; a word mixing directions takes its first
  character's level.
- Decorations do not span the spaces between words.
- `letter-spacing`, `word-spacing`, `overflow-wrap`, `word-break`,
  `text-overflow`, `font-variant`, `font-feature-settings`, `font-stretch`,
  vertical text, colour emoji and dictionary line breaking (Thai, Lao,
  Khmer) → backlog.
