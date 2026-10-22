//! Resolves a `dom::Element`'s computed style via a real cascade
//! (Milestone 4) and a complete CSS box model (Milestone 6: padding,
//! border, `box-sizing` — Milestones 1-5 only had width/height/margin).
//!
//! Cascade order implemented (lowest to highest priority — later
//! passes overwrite earlier ones), the standard CSS cascade for an
//! author-only stylesheet + inline style:
//!   1. stylesheet rules, normal declarations (by specificity, then
//!      source order)
//!   2. inline `style="..."` normal declarations (always beats any
//!      stylesheet normal declaration, regardless of specificity —
//!      this is standard CSS behavior, not a simplification)
//!   3. stylesheet rules, `!important` declarations (by specificity,
//!      then source order)
//!   4. inline `style="..."` `!important` declarations (highest of all)
//!
//! Selector support: type, class, ID, and descendant-combinator
//! compounds only (`div.foo bar#baz`) — no child/sibling combinators,
//! attribute selectors, or pseudo-classes yet.
//!
//! Inheritance: only `color` and `text-align` inherit — box-model
//! properties (margin, padding, border, width, height) never inherit
//! in real CSS either, so this needs no special-casing beyond what's
//! already here.
//!
//! Box model (Milestone 6): `margin`/`padding` support the standard
//! 1/2/3/4-value shorthand plus longhands (`margin-top` etc.). Border
//! is `border-width`/`border-color`/`border-style` (only `solid` is a
//! real style — anything else behaves as `none`, i.e. no border
//! renders) plus a `border` shorthand. `box-sizing: content-box |
//! border-box` changes what `width`/`height` measure. See
//! milestones/milestones-01-17.md for exact scope — no
//! `border-radius`, no per-side border colors/styles, no
//! `box-shadow`/`outline`.
//!
//! Internally split into: `values` (plain CSS value types), `style`
//! (the `Style` struct + `StyleCache`), `transitions` (`transition`/
//! `@keyframes` animation), `cascade` (selector-candidate indexing,
//! specificity/priority resolution, the parallel whole-tree entry
//! point), `declarations` (applying one resolved declaration onto a
//! `Style`), and `selectors` (selector/combinator matching). This is a
//! pure internal reorganization — the public API below is unchanged
//! from before the split.

mod cascade;
mod color;
mod content;
mod declarations;
mod grid;
mod incremental;
mod length;
mod selectors;
mod style;
pub mod text;
mod transitions;
mod user_agent;
mod values;

pub use cascade::{
    resolve_style, resolve_style_in_context, resolve_style_tree_in_viewport, resolve_style_tree_parallel, Interaction, MatchContext,
    DEFAULT_VIEWPORT_HEIGHT, DEFAULT_VIEWPORT_WIDTH,
};
pub use user_agent::{user_agent_stylesheet, USER_AGENT_CSS};
pub use content::{is_pseudo, owner_of, pseudo_id, resolve_content, PseudoElement, MAX_CONTENT_CHARS};
pub use grid::{GridStyle, GridLine, GridPlacement, TrackBreadth, TrackList, TrackSize, MAX_GRID_LINE, MAX_GRID_TRACKS};
pub use color::{parse_color, MAX_COLOR_INPUT};
pub use length::{parse_length, LengthContext, ViewportSize, MAX_CALC_DEPTH, MAX_LENGTH_INPUT};
pub use incremental::{AnimationReport, RestyleReport, SheetFeatures, StyleEngine};
pub use style::{SizeLimits, Style, StyleCache};
pub use text::{Decoration, FontStyleKind, LineHeight, TextStyle, TextTransform, UnicodeBidi};
pub use transitions::{apply_animations, apply_animations_to, apply_transitions, apply_transitions_to, AnimationRegistry, Easing, TransitionProperty, TransitionRegistry, TransitionSpec};
pub use values::{
    AlignContent, AlignItems, BackgroundPosition, BackgroundRepeat, BackgroundSize, BoxShadow, Clear, Float, Overflow, TableBorders, Position, Visibility, BorderSide, BorderSpec, BorderStyle, BoxSizing, Color, Display, EdgeSizes, Edges,
    FlexDirection, FlexWrap, IntrinsicKeyword, JustifyContent, Length, ObjectFit, TextAlign, Transform, VerticalAlign,
    WhiteSpace, initial_display, MAX_BOX_SHADOWS, MAX_FONT_SIZE_PX, MAX_SHADOW_EXTENT,
};

// Only for `tests.rs`'s `use super::*` (a child module inherits its
// parent's private `use` bindings, same as the pre-split single-file
// layout relied on) — not part of the crate's own public API.
#[cfg(test)]
use css_parser::{Declaration, KeyframesRule, Stylesheet};
#[cfg(test)]
use dom::Element;

#[cfg(test)]
mod tests;
