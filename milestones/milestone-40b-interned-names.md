# Milestone 40b — Interned names

> **Status:** Done (2026-10-21). **Still open:** attribute *values* — `class`
> and `id` values — are still strings, so a class selector still compares
> text (interning them is a decision of its own) → backlog; most of a
> restyle is applying declarations, whose property names are matched as
> strings (architecture review A6) → backlog, where it already was. A page's
> own table only grows during the document's life (a script that invents and
> drops names keeps their entries until the document goes). Decision record:
> `docs/adr/024-interned-names.md` (implementing ADR 021 §5). Overall status:
> `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** names compared as integers where they are compared most (ADR 021
§5), measured as a gain — after M39 measured interning *after* parsing as a
loss (first frames +7%, clicks +60%, branch `m39-atoms-measured`).

## What was built

1. **A `names` crate** (ADR 024 §1–3): `Atom`, `Name`, the well-known table
   `STATIC` (233 names: the HTML elements, the common SVG elements and the
   attributes the corpus uses most, ranked by count over its 56 pages) and
   `AtomTable`, a document's own table. Dependency-free and core;
   `dom`, `html_parser` and `css_parser` share it, and `dom` re-exports it
   (`dom::atoms::DIV`). A `Name` is two words — an atom, or its own text
   boxed twice — and reads as a `str`, so `el.tag == "p"` and
   `("id".into(), …)` compile unchanged: about ten lines of test code needed
   editing.
2. **Names born interned** (§4). The tokenizer makes a `Name` from its name
   buffers, which it now keeps across tags instead of taking (one allocation
   per tag name and per attribute name before; none for a well-known name
   now). `createElement` and the CSS parser make `Name`s the same way.
3. **Interned in place in the document** (§5). `Document::new` and every
   insertion turn each name that is not well-known into an atom of the
   document's table *in place* — no attribute list rebuilt, nothing done for
   a well-known name; `setAttribute` interns through the same table;
   `removeAttribute` of a name the document never saw does not intern it.
   Removed and cloned subtrees take their names out as text
   (`AtomTable::detach`). `Document::invented_names` counts the table.
4. **Readers compare atoms** (§6). `ElementRef::is(atom)`,
   `attr_atom(atom)`, `well_known_tag()`, `has_tag(&Name)`,
   `attr_named(&Name)`, and `is`/`attr_atom` on `ElementMut`; the cascade's
   `id`, `class`, `style`, `hidden`, `type`, `dir` and presentational reads,
   and every `attr("…")`/`tag() == "…"` with a literal name in layout,
   engine, accessibility and the script host use them.
5. **Selectors match by atom** (§7). Type and attribute selectors hold
   `Name`s; `has_tag` is one integer comparison for a well-known type; the
   rule index keys rules with a well-known subject type by atom in a vector
   (an index per element, not a string hash); an element's initial
   `display` is a table read at its tag's atom, built once from the string
   match it replaces; the legacy table hints and size hints test the tag's
   atom.
6. **Benchmarks** that use only pre-M40b API, so each measures M40 too:
   `crates/style/examples/restyle_bench.rs` (a selector-heavy restyle) and
   `crates/html_parser/examples/parse_bench.rs` (parse and `Document::new`
   over a directory of real pages).

## Acceptance criteria

- [x] **`frame_bench` first frames and DOM-change frames on the 10k pages
      are no slower than M40, and selector-heavy restyles faster.** First
      frames −7.0% and −7.5%, DOM changes −10.1% on both 10k pages; the
      selector-heavy restyle −3% to −6% (tables below). *The machine was not
      quiet* (load average 3–7: KDE's file indexer working through the build
      output, while the 16 cores were ~89% idle) — the same conditions as
      M40's measurements, with interleaved runs, so the comparison holds; a
      quiet-machine re-run would tighten the spreads.
- [x] **A page inventing a million attribute names grows only its own
      document's table, freed with it (counted).**
      `invented_names_grow_only_their_own_documents_table`: 100,000 elements
      with ten invented names each — 1,000,001 entries in that document's
      table (the names and one custom tag; `class` and `div` add none),
      another document's table at 0, and a document made after the big one
      is dropped at 0. `script_names_are_interned_and_survive_removal`
      counts `setAttribute`'s interning (once per name) through removal and
      re-insertion.
- [x] **The restyle and layout oracles and the corpus goldens pass
      unchanged.** The oracles are in the test suite (all pass); the 56
      corpus pages render and match their goldens, none re-blessed.

## Verification

Standing Definition of Done: 1,164 tests, 0 failures (up from 1,156: 6 in
`names`, 2 in `dom`); `cargo build --workspace --release` clean; clippy at 28
(unchanged); every guardrail and `test-guardrails.sh` pass (`names` is listed
in `scripts/core-crates.txt`); the fuzz suite passes — no new parser, but the
`parse_document` target now also moves the tree into a `Document` and asserts
every name reads back as parsed, with a new seed, `14_invented_names.html`
(3,000 invented tags and attributes, upper-case names, a 100,000-character
attribute name and a 70,000-character tag name; in `generate_seeds.py`). No
new renderer message, so no new `ipc_decode` seeds.

AddressSanitizer, CI's `sanitizer` command with `names`, `dom`,
`html_parser`, `css_parser` and `style` added, 3 rounds: 0 reports, 0
failures.

## Measurements

Interleaved runs of M40 (`f50eb04`) and M40b, binaries built from each; load
average 3–7 throughout.

`frame_bench`, 40 rounds (median of medians; best in brackets):

| Page | Frame | M40 | M40b | Change |
|---|---|---|---|---|
| demo | first | 3.727 (3.046) ms | 3.693 (3.251) ms | −0.9% |
| | scroll / DOM change / caret | 0.102 / 0.066 / 0.063 | 0.103 / 0.065 / 0.063 | +1% / −2% / 0% |
| 1k rows | first | 7.153 (6.680) ms | 6.910 (6.175) ms | −3.4% |
| | scroll / DOM change / caret | 0.025 / 0.098 / 0.018 | 0.028 / 0.095 / 0.017 | +12% / −3% / −6% |
| 10k rows | first | 55.39 (52.98) ms | 51.49 (49.17) ms | −7.0% |
| | scroll / DOM change / caret | 0.053 / 0.333 / 0.025 | 0.047 / 0.299 / 0.025 | −11% / −10% / −2% |
| 10k + flex/vars | first | 63.80 (61.84) ms | 59.04 (56.69) ms | −7.5% |
| | scroll / DOM change / caret | 0.043 / 0.331 / 0.024 | 0.043 / 0.297 / 0.024 | −1% / −10% / 0% |

Scroll frames swing by ±10–25% between runs with unchanged bests (an earlier
30-round run had the 1k page −0% and the 10k page +25%; this one has +12% and
−11%): they are tens of microseconds and do no name work, so their medians
are noise here. Every other cell repeated across two runs.

Selector-heavy restyle (`restyle_bench`: 7,206 elements, 529 type, class,
attribute and descendant rules), 16 rounds of 60:

| | M40 median (best) | M40b median (best) | Change |
|---|---|---|---|
| first restyle | 3.543 (2.812) ms | 3.333 (2.660) ms | −5.9% (−5.4%) |
| class toggle on `<body>` (every element restyled) | 5.342 (3.994) ms | 5.178 (3.764) ms | −3.1% (−5.8%) |

The gain is small because most of a restyle is not name work: applying each
matched declaration matches its property name as a string (A6), and class
selectors compare class *values*, which stay text.

Parsing the 56 corpus documents (12 MB; `parse_bench`, 10 rounds of 15):
parse −6.9% to −9.5%, parse plus `Document::new` −6.9% to −9.8% (two runs).
These pages invent many names (`data-mw`, `data-title`, MDN's custom
elements), so the in-place interning of names that are not well-known is in
this measurement, and is outweighed by the allocations the well-known ones
no longer make.

## Not done / open

- **Attribute values** (`class`, `id`) are strings; class selectors compare
  text → backlog.
- **Declaration application by string** (A6) is now the larger share of a
  restyle → backlog (already there).
- **A page's own table only grows** while its document lives; bounded by the
  document's life, not by count.
- **`parse_document` filling the document's table directly** would save the
  boxed text of invented names; not needed for the gain (ADR 024,
  Alternatives).
