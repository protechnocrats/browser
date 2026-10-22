//! The `Style` struct itself — every computed CSS property this engine
//! models, as one flat aggregate — plus `StyleCache`, the
//! `dom::element_ptr_key`-keyed map that a whole resolved tree's worth
//! of `Style`s lives in between cascade, transitions/animations, and
//! layout.

use crate::values::{
    AlignContent, AlignItems, BackgroundPosition, BackgroundRepeat, BackgroundSize, BoxShadow, Clear, Float, Overflow, TableBorders, Position, Visibility, BorderSide, BorderSpec, BorderStyle, BoxSizing, Color, Display,
    EdgeSizes, Edges, FlexDirection, FlexWrap, IntrinsicKeyword, JustifyContent, Length, ObjectFit, TextAlign, Transform,
    VerticalAlign, WhiteSpace,
};
use crate::transitions::TransitionSpec;
use std::collections::HashMap;

/// Not `Copy` — `background_image_path` is a `String`. Every existing
/// use only ever reads individual Copy sub-fields (`.width`,
/// `.margin`, `.color`, etc.) rather than duplicating the whole
/// struct, so this doesn't ripple out; `resolve_style` still returns
/// one owned `Style` by value as always.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub width: Option<Length>,
    pub height: Option<Length>,
    /// Milestone 33: `min-`/`max-width`/`-height`, boxed (rarely set; read
    /// through [`Style::limits`]).
    pub size_limits: Option<Box<SizeLimits>>,
    /// Milestone 33: `position`, its insets (boxed, read through
    /// [`Style::inset`]; `None` in each is `auto`) and `z-index` (`None`:
    /// `auto`).
    pub position: Position,
    pub insets: Option<Box<Edges<Option<Length>>>>,
    pub z_index: Option<i32>,
    /// Milestone 33: whether this element's `::before` / `::after`
    /// generate a box (their styles are in the cache under their pseudo
    /// ids) — so layout need not look for ones that do not exist.
    pub generates_before: bool,
    pub generates_after: bool,
    /// Milestone 33: `overflow-x` / `overflow-y`.
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    /// Milestone 33: `opacity` (0..=1, not inherited, but it applies to
    /// the whole subtree) and `visibility` (inherited).
    pub opacity: f32,
    pub visibility: Visibility,
    /// Milestone 33: `box-shadow`, first shadow on top.
    pub box_shadow: Vec<BoxShadow>,
    /// Milestone 33: `float` and `clear`.
    pub float: Float,
    pub clear: Clear,
    /// Milestone 33: `border-collapse` and `border-spacing` (inherited).
    pub table_borders: TableBorders,
    /// Milestone 33: the gap between rows (`gap` is between columns — and
    /// between a row flex container's items).
    pub row_gap: f32,
    /// Milestone 33: the CSS Grid properties, boxed — few elements set
    /// any, and every element's `Style` is built and moved per restyle.
    /// `None` is every grid property at its initial value; read through
    /// [`Style::grid`].
    pub grid: Option<Box<crate::grid::GridStyle>>,
    pub background_color: Color,
    /// Milestone 33: computed values — `None` is `auto`; percentages
    /// resolve against the containing block's width in layout.
    pub margin: Edges<Option<Length>>,
    pub padding: Edges<Length>,
    /// M6's gap: each side's width, style and colour as declared — boxed
    /// and `None` without any border declaration, the usual case (ADR 014's
    /// rare properties: a `Style` is cloned for every element). Read it
    /// through [`Style::border`].
    pub border_spec: Option<Box<Edges<BorderSpec>>>,
    /// Phase C: uniform corner radius (one length; per-corner is a
    /// documented further extension).
    pub border_radius: f32,
    /// Phase C: `outline` — painted just outside the border box, never
    /// affects layout. `outline_style == None` means no outline.
    pub outline_width: f32,
    pub outline_color: Color,
    pub outline_style: BorderStyle,
    pub outline_offset: f32,
    pub box_sizing: BoxSizing,
    /// Computed font size in pixels (inherited). Milestone 33: every
    /// unit is resolved at cascade time, so this is always pixels.
    pub font_size: f32,
    /// The root element's font size — what `rem` means for this element
    /// (inherited unchanged from the root down).
    pub root_font_size: f32,
    pub color: Color,
    /// Milestone 35: the inherited text properties, shared with the parent
    /// until this element declares one ([`crate::text`]).
    pub text: std::sync::Arc<crate::text::TextStyle>,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    /// Phase B: `vertical-align` for this element when it participates
    /// in an inline formatting context. Only the keyword values are
    /// modelled (no length/percentage offsets).
    pub vertical_align: VerticalAlign,
    /// Milestone 7: the raw path/URL from `background-image: url(...)`.
    /// Just the string — decoding and caching the actual image is
    /// `layout`'s job (it's the crate that already needs the `images`
    /// crate for `<img>` intrinsic sizing; keeping `style` free of
    /// that dependency matches its existing minimal-dependency
    /// posture). Network URLs will decode to `None` until Milestone 8
    /// — this crate has no way to know the difference between "a
    /// path" and "a URL" yet, `layout`'s file-read will just fail
    /// gracefully for the latter (see milestones/milestones-01-17.md).
    pub background_image_path: Option<String>,
    /// Milestone 10: flexbox. `display`/`flex_direction`/
    /// `justify_content`/`align_items`/`gap` are container-side
    /// properties (meaningful when `display: flex` is set on the
    /// element with children); `flex_grow`/`flex_shrink`/`flex_basis`
    /// are item-side properties (meaningful on a child of a flex
    /// container). None of these inherit — real CSS doesn't inherit
    /// any flex property either.
    pub display: Display,
    pub flex_direction: FlexDirection,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub gap: f32,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Option<Length>,
    /// Phase D: `flex-wrap` (container), `align-content` (container),
    /// `order` (item). `order` participates in nothing but flex-item
    /// ordering; default 0, may be negative.
    pub flex_wrap: FlexWrap,
    pub align_content: AlignContent,
    pub order: i32,
    /// Phase D: `min-content` / `max-content` / `fit-content` keyword
    /// for `width` / `height`. When set it takes precedence over the
    /// length/auto value in `width` / `height`.
    pub width_intrinsic: Option<IntrinsicKeyword>,
    pub height_intrinsic: Option<IntrinsicKeyword>,
    /// Phase E: `object-fit` for `<img>`, `background-size` /
    /// `background-position` for `background-image`.
    pub object_fit: ObjectFit,
    pub background_size: BackgroundSize,
    pub background_position: BackgroundPosition,
    pub background_repeat: BackgroundRepeat,
    pub transform: Transform,
    /// Milestone 34: `will-change: transform` — the author says the
    /// transform will change, so the element gets a transform layer of its
    /// own up front (see [`Style::composited_transform`]).
    pub will_change_transform: bool,
    /// Milestone 22: parsed `transition` shorthand entries — never
    /// inherited (matches real CSS; not listed in the `Default` impl's
    /// inheritance path below). Consumed by `apply_transitions`, not
    /// by layout/rendering directly.
    pub transitions: Vec<TransitionSpec>,
    /// Milestone 22: `animation-name` — `None` means "not animating."
    /// Never inherited, same as `transitions`. Only a single name is
    /// supported (real CSS allows a comma-separated list running
    /// several animations at once); a documented scope cut, see
    /// `milestone-22-animation-depth.md`.
    pub animation_name: Option<String>,
    /// Milestone 22: `animation-duration` in seconds. Defaults to 0.0
    /// (no animation, matching real CSS) when `animation-name` is set
    /// but no duration was given.
    pub animation_duration: f32,
    /// Milestone 22: `animation-iteration-count` — `None` means
    /// `infinite`; `Some(n)` is a finite repeat count. Defaults to
    /// `Some(1.0)`, matching real CSS.
    pub animation_iteration_count: Option<f32>,
    /// M22's gap: `animation-timing-function` (default `ease`, as in CSS) —
    /// how each keyframe segment is paced, unless its stop sets its own.
    pub animation_timing_function: crate::transitions::Easing,
    /// Phase A: CSS custom properties (`--foo: value`). Inherited (a
    /// child shares its parent's map — copied on write), then overridden
    /// by any `--foo` declarations that match the child. `var(--foo)`
    /// references in other declarations are substituted against this
    /// map during resolution, so by the time layout sees a `Style`
    /// there are no unresolved `var()`s left in the typed fields.
    pub custom_properties: std::sync::Arc<HashMap<String, String>>,
    /// Phase A: the raw `content` value from a `::before`/`::after`
    /// rule is parsed here when it lands on an element's own style
    /// (rare, but valid) — `layout` does not yet generate the pseudo-
    /// element boxes that would actually display it. Kept so the
    /// property round-trips rather than being silently dropped.
    pub content: Option<String>,
}

/// `min-width`/`min-height` (`None`: `auto`) and `max-width`/`max-height`
/// (`None`: `none`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SizeLimits {
    pub min_width: Option<Length>,
    pub min_height: Option<Length>,
    pub max_width: Option<Length>,
    pub max_height: Option<Length>,
}

impl Style {
    /// Milestone 34: whether the element's `transform` may change without
    /// anything else changing — `will-change: transform`, an animation, or
    /// a transition on `transform` — so it is painted on a transform layer
    /// of its own that a frame can move without a new display list.
    pub fn composited_transform(&self) -> bool {
        self.will_change_transform
            || self.animation_name.is_some()
            || self
                .transitions
                .iter()
                .any(|t| matches!(t.property, crate::TransitionProperty::Transform | crate::TransitionProperty::All))
    }

    /// `min-`/`max-width`/`-height`.
    /// Each side's width, style and colour, an unset colour being the
    /// element's own (`currentcolor`).
    pub fn border(&self) -> Edges<BorderSide> {
        let current = self.color;
        match self.border_spec.as_deref() {
            None => Edges::all(BorderSide { color: current, ..BorderSide::default() }),
            Some(spec) => spec.map(|s| BorderSide { width: s.width, style: s.style, color: s.color.unwrap_or(current) }),
        }
    }

    /// Each side's used width (0 without a style) — without building the
    /// sides, and nothing at all to do for an element with no border.
    #[inline]
    pub fn border_widths(&self) -> EdgeSizes {
        match self.border_spec.as_deref() {
            None => EdgeSizes::default(),
            Some(spec) => spec.map(|s| if s.style == BorderStyle::None { 0.0 } else { s.width.max(0.0) }),
        }
    }

    /// Whether any side has a border.
    #[inline]
    pub fn has_border(&self) -> bool {
        let w = self.border_widths();
        w.top > 0.0 || w.right > 0.0 || w.bottom > 0.0 || w.left > 0.0
    }

    /// The declared border, to change.
    pub fn border_mut(&mut self) -> &mut Edges<BorderSpec> {
        self.border_spec.get_or_insert_with(Default::default)
    }

    pub fn limits(&self) -> SizeLimits {
        self.size_limits.as_deref().copied().unwrap_or_default()
    }

    pub fn limits_mut(&mut self) -> &mut SizeLimits {
        self.size_limits.get_or_insert_with(Default::default)
    }

    /// `top`/`right`/`bottom`/`left` (`None`: `auto`).
    pub fn inset(&self) -> Edges<Option<Length>> {
        self.insets.as_deref().copied().unwrap_or(Edges::all(None))
    }

    pub fn inset_mut(&mut self) -> &mut Edges<Option<Length>> {
        self.insets.get_or_insert_with(|| Box::new(Edges::all(None)))
    }

    /// The grid properties (initial values when none were set).
    pub fn grid(&self) -> &crate::grid::GridStyle {
        static INITIAL: std::sync::OnceLock<crate::grid::GridStyle> = std::sync::OnceLock::new();
        self.grid.as_deref().unwrap_or_else(|| INITIAL.get_or_init(crate::grid::GridStyle::default))
    }

    /// The grid properties, to set one.
    pub fn grid_mut(&mut self) -> &mut crate::grid::GridStyle {
        self.grid.get_or_insert_with(Default::default)
    }
}

impl Default for Style {
    fn default() -> Self {
        Style {
            width: None,
            height: None,
            size_limits: None,
            position: Position::Static,
            insets: None,
            z_index: None,
            generates_before: false,
            generates_after: false,
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            opacity: 1.0,
            visibility: Visibility::Visible,
            box_shadow: Vec::new(),
            float: Float::None,
            clear: Clear { left: false, right: false },
            table_borders: TableBorders { collapse: false, spacing: (0.0, 0.0) },
            row_gap: 0.0,
            grid: None,
            background_color: Color::TRANSPARENT,
            margin: Edges::all(Some(Length::ZERO)),
            padding: Edges::all(Length::ZERO),
            border_spec: None,
            border_radius: 0.0,
            outline_width: 0.0,
            outline_color: Color { r: 0, g: 0, b: 0, a: 255 },
            outline_style: BorderStyle::None,
            outline_offset: 0.0,
            box_sizing: BoxSizing::ContentBox,
            font_size: 16.0,
            root_font_size: 16.0,
            color: Color { r: 0, g: 0, b: 0, a: 255 },
            text: crate::text::default_text(),
            text_align: TextAlign::Start,
            white_space: WhiteSpace::Normal,
            vertical_align: VerticalAlign::Baseline,
            background_image_path: None,
            display: Display::Block,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            flex_wrap: FlexWrap::Nowrap,
            align_content: AlignContent::Stretch,
            order: 0,
            width_intrinsic: None,
            height_intrinsic: None,
            object_fit: ObjectFit::Fill,
            background_size: BackgroundSize::Auto,
            background_position: BackgroundPosition::default(),
            background_repeat: BackgroundRepeat::Repeat,
            transform: Transform::default(),
            will_change_transform: false,
            transitions: Vec::new(),
            animation_name: None,
            animation_duration: 0.0,
            animation_iteration_count: Some(1.0),
            animation_timing_function: crate::transitions::Easing::EASE,
            custom_properties: std::sync::Arc::default(),
            content: None,
        }
    }
}

/// Precomputed styles, keyed by `element.node_id`. See
/// `resolve_style_tree_parallel`'s doc comment for why this exists
/// and what it's safe to assume about the keys' lifetime.
pub type StyleCache = dom::NodeIdMap<Style>;
