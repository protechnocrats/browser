# ADR 024 — Interned names

- **Status:** accepted (Milestone 40b, 2026-10-21)
- **Decides:** how tag and attribute names are represented from the
  tokenizer to selector matching; where they are interned; which table owns
  a name a page invents.
- **Constrains:** `names` (new), `dom` (`Element`'s `tag` and `attributes`,
  the arena), `html_parser` (`Token`), `css_parser` (`CompoundSelector::tag`,
  `AttrSelector::name`), `style` (the rule index and matching), and every
  reader that compares a name it knows (`attr_atom`, `is`).
- **Implements:** ADR 021 §5, deferred by M39 and split out of M40.

## Context

ADR 021 §5 designed interned names — a fixed table of well-known names
equal in every document, a per-document table for the rest, integer
comparison where a known name is looked up — and M39 built it on branch
`m39-atoms-measured`. Measured, it lost: names were born as `String`s in the
tokenizer and interned when `Document::new` moved the parsed tree into the
arena, so each name was allocated, hashed, freed, and each attribute list
rebuilt at a smaller element size. First frames on 10,000 rows were 7%
slower; clicks, whose hit test walks the whole box tree, about 60% slower
from the heap holes the freed strings left. The conclusion then: intern
where names are born, and match selectors by atom.

## Decision

1. **A crate of its own, `names`.** `Atom`, `Name`, the well-known table
   and `AtomTable` live in a dependency-free core crate that `dom`,
   `html_parser` and `css_parser` share, so the CSS parser names a type
   selector the way the DOM names a tag without depending on `dom`. `dom`
   re-exports them (`dom::atoms::DIV`, `dom::Name`).
2. **Well-known names are atoms everywhere.** `names::STATIC` lists the
   HTML elements, the common SVG elements and the attributes the real-site
   corpus uses most (ranked by count across its 56 pages); an atom below
   `STATIC_COUNT` is one of them, with the same value in every document and
   every process. Looking one up is a read of a fixed hash table (FNV; safe
   because page content can only look up, never insert) and is skipped for
   text longer than the longest well-known name.
3. **A `Name` is an atom or its own text.** `Name` is two words: an `Atom`,
   or a boxed `Box<str>` (boxed twice so the rare case does not widen the
   common one). It reads as a `str` (`Deref`, `PartialEq<str>`, `Hash` as
   its text), so code and tests that compared `el.tag == "p"` or pushed
   `("id".into(), …)` are unchanged. Equality of two names is an integer
   comparison when both are atoms.
4. **Names are born interned.** The tokenizer makes a `Name` from its name
   buffer (kept across tags, no longer taken) — a well-known name allocates
   nothing; `createElement` and the CSS parser do the same.
5. **A document interns its own names in place.** The arena keeps the
   fragment's `Vec<(Name, String)>` as it is; `Document::new` and every
   insertion turn each name that is not well-known into an atom of the
   document's `AtomTable`, in place (`AtomTable::intern_name`) — no
   attribute list is rebuilt, and a page of well-known names is not touched.
   `setAttribute` interns through the same table. The table belongs to the
   document and only grows during its life: a page inventing a million names
   grows its own table, freed with it, and nothing global
   (`Document::invented_names`, tested). A document's own atom never leaves
   it: a removed or cloned subtree's names go out as text
   (`AtomTable::detach`), and `Name::as_str` of a document's atom outside its
   table is a debug assertion.
6. **Readers compare atoms where they know the name.** `ElementRef::is(atom)`,
   `attr_atom(atom)`, `well_known_tag()`, and `has_tag`/`attr_named` for a
   selector's `Name` (an atom compared as an integer; a name that is not
   well-known compared as text, and never equal to a well-known one). The
   cascade's `id`/`class`/`style`/`hidden`/presentational reads, the tag
   checks in layout, the engine, accessibility and the script host use them.
   `attr(&str)` stays for names given as text.
7. **Selectors match by atom.** A type selector's and an attribute
   selector's name are `Name`s; the rule index keeps rules keyed by a
   well-known type in a vector indexed by atom (an index per element instead
   of a string hash), and other types in the hash map as before. An
   element's initial `display` for a well-known tag is a table read at its
   atom, built once from the string match it replaces.

## Consequences

- Measured against M40 (interleaved runs; the milestone file has the
  tables): the 56 corpus documents parse 7–10% faster, parse plus
  `Document::new` likewise; `frame_bench` first frames on the 10,000-row
  pages 7–8% faster and DOM-change frames 10–11% faster; a selector-heavy
  restyle (7,206 elements, 529 rules) 3–6% faster. The restyle gain is
  small because most of a restyle is applying declarations, whose property
  names are still matched as strings (architecture review A6).
- Attribute *values* — `class` and `id` values — are still strings; a
  class selector compares text. Interning class names is a separate
  decision (it would need the per-document table on the selector side).
- `css_parser` depends on `names`. A name that is not well-known costs two
  allocations in a fragment (the boxed text) instead of one, and one more
  when it is interned; the corpus, whose pages invent many (`data-*`,
  custom elements), still parses faster overall.
- Comparing two `Name`s from different documents' tables is meaningless;
  only well-known atoms and text cross a document boundary, which the
  API enforces (`Name`'s representation is private to `names`).

## Alternatives

- **Intern in `Document::new`** (M39): measured as a loss (above).
- **The parser fills the document's table directly** (`parse_document`
  returning the table with the tree): saves the boxed text of invented names,
  but an owned tree's names would then be meaningless without its table,
  breaking every reader of a parsed tree outside a document (tests, layout's
  helpers). Not needed for the measured gain.
- **One process-global table:** a page could grow memory that outlives it,
  and tables would need locking across page threads. ADR 021 §5 ruled it
  out.
- **`String` names with a cached hash:** keeps the allocations the
  tokenizer now avoids and gives no integer comparison.
