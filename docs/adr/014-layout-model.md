# ADR 014 — Layout model: document root, user-agent origin, generated content, stack contract

- **Status:** accepted (Milestone 33)
- **Decides:** what the styled/laid-out document is, where default styles come
  from, how generated content is identified, how rarely-used properties are
  stored, which float semantics the engine implements, and how much stack
  layout may need.
- **Constrains:** every future CSS feature (M34, M35), the DOM's id space,
  every host that lays out a document, every change to `Style` or
  `LayoutBox`.

## Context

Before Milestone 33 the engine laid out `<body>` (so `:root` and `html { … }`
rules could not apply), had no default styles beyond a few tag checks in
layout, never generated `::before`/`::after`, and handled only static, block
and flex layout. Milestone 33 added units, the UA stylesheet, `display`,
min/max sizes, backgrounds, positioning, overflow, opacity/visibility,
`box-shadow`, generated content, floats, tables and grid. Several of those
forced decisions that later work must keep to.

## Decision

1. **The document is `<html>`.** `engine::Page` builds its `dom::Document`
   from the whole parsed root (the parser always implies `<html>`, `<head>`,
   `<body>`). `<head>` is `display: none`. `:root` matches the element with no
   parent. The canvas colour is the root's background, else `<body>`'s
   (CSS 2 §14.2); the root's and body's `overflow` belong to the viewport.
2. **Default styles are a user-agent stylesheet in a cascade origin.**
   `css_parser::Rule` carries a `CascadeOrigin` (`UserAgent` / `Author`);
   the cascade sorts normal declarations by origin first (a UA rule loses to
   any author rule whatever the specificity) and applies UA `!important`
   last. The sheet (`style/src/user_agent.css`) may only use properties the
   engine implements (a test enforces it, so it never pollutes the
   conformance report). Each tag's initial `display` is built into the
   cascade (`style::initial_display`) so every styling path agrees; HTML
   presentational attributes apply between the UA sheet and author rules.
3. **Generated content has derived node ids.** A `::before`/`::after` that
   generates a box has its style in the `StyleCache` under its owner's id
   with a reserved high bit (`style::pseudo_id`); `dom` allocates element ids
   only below `dom::MAX_ELEMENT_NODE_ID` (checked). Layout inserts a
   synthetic element for it, so every layout mode handles it unchanged; hit
   testing reports the owner; a pseudo-element's change is its owner's.
4. **Hot structs stay small; rare groups are boxed.** `Style` (built and
   moved per element per restyle) and `LayoutBox` (one per word of text,
   walked every painted frame) keep rarely-set property groups behind
   `Option<Box<…>>` with accessors: `Style::{grid, limits, inset}`,
   `LayoutBox::extras`, `DisplayItem::Image::sampling`. A test bounds
   `Style` at 512 bytes. A new property group that most elements never set
   goes behind the same kind of box.
5. **Floats: a block container always contains its floats.** Floats,
   exclusions, `clear` and formatting-context roots follow CSS 2 §9.5, with
   one simplification: every block container grows to hold the floats it
   placed (what a clearfix does), where CSS lets a float hang out of a
   parent that is not a formatting-context root. Nothing laid out beside
   floats is reused.
6. **Layout has a stack contract.** Layout, painting and hit testing are
   recursive to `MAX_LAYOUT_DEPTH` (512). `layout::MIN_STACK_BYTES`
   (8 MiB) is the stack a thread must give them; measured need at that
   depth is ~1.5 MiB (block), ~2 MiB (flex), ~3 MiB (grid). The engine
   asserts at compile time that page threads (16 MiB) provide it;
   `integration_tests/tests/stack_depth.rs` checks every layout mode fits in
   half of it; fuzz targets that lay out documents run on a thread of that
   size.

## Alternatives considered

- **UA defaults hard-coded in layout** (as before): kept each rule's
  reasoning local but made them unoverridable by author CSS and invisible to
  the cascade. Rejected.
- **A separate UA stylesheet passed alongside the author sheet**: needs a
  second index and a merge in every cascade entry point; one sheet with an
  origin per rule is one sort key.
- **Generated content as real DOM nodes**: would make scripts see them and
  every DOM mutation path handle them. Rejected.
- **Converting layout to an explicit stack** (no recursion): the robust
  answer to the stack contract, but a rewrite of every layout mode; the
  contract plus the guard test is enough while depth is capped at 512.

## Consequences

- Pages look like they do in browsers by default (margins, heading sizes,
  lists, links), and `html { font-size: 62.5% }`-style sites size correctly.
- Engine tests that position boxes see body's 8px margin.
- A struct change that grows `Style` past 512 bytes fails a test; a layout
  change that deepens recursion frames fails the stack-depth test.
- Float-heavy legacy layouts that relied on floats overflowing a parent
  render taller than in browsers.
