# Resolved (Milestone 35): `fontdue` 0.8 hung on a corrupted `cmap` table

> **Resolved 2026-09-28.** `fontdue` was replaced by `ttf-parser` + `rustybuzz`
> and our own rasterizer (ADR 009). The reproducer is now a passing regression
> test, `text::tests::the_corrupted_cmap_that_hung_fontdue_is_handled_promptly`,
> and a seed of the `font_parse` and `web_font` fuzz targets
> (`07_fontdue_cmap_hang.ttf`, `03_fontdue_cmap_hang.ttf`). The record below is
> kept as it was.

- **Found:** 2026-09-24, Milestone 32, by `corrupted_copies_of_a_real_font_never_panic`
  (`FUZZ_FONT_ROUNDS=305` reaches it: round 304).
- **Reproducer:** the bundled `crates/text/assets/DejaVuSans.ttf` with the single
  byte at file offset **50864** changed from `0x00` to `0xa8` (byte 2076 of the
  `cmap` table). `text::Font::from_bytes` — i.e. `fontdue::Font::from_bytes`, which
  builds its character map eagerly at load — never returns (> 10 hours observed).
  The other five bytes corrupted in that round are harmless on their own.
- **Kind:** a hang, not a panic: `catch_unwind` (ADR 007) cannot contain it, and a
  page thread stuck in it could only be abandoned, not stopped.
- **Reachable from web content today?** No. The engine loads only its bundled font;
  `@font-face` rules are parsed (M32) but their fonts are never fetched or parsed.
- **Consequence:** this **blocks Milestone 35 (web fonts)** as specified: web fonts
  must not ship on a font parser with a known unbounded-time input. ADR 009 (M35's
  shaping/font-parsing dependency choice) must pick a parser that survives this
  input (candidates: a current `ttf-parser`, which parses lazily per glyph, used
  directly or via `swash`/`rustybuzz`), and this reproducer becomes a committed
  regression test there.
- **Regression test now:** `text::tests::known_issue_fontdue_hangs_on_a_corrupted_cmap`
  (ignored — it demonstrates the hang with a watchdog; run it with `--ignored`).
