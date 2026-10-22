//! Plain CSS value types — colors, lengths, box-model edges, and every
//! keyword-style enum (`display`, `flex-*`, `object-fit`,
//! `background-*`, `text-align`, `white-space`, `vertical-align`, ...).
//! No cascade/selector logic lives here; just the vocabulary a `Style`
//! (see `crate::style`) is built out of.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
}

/// Largest `font-size` (px) the cascade will produce. Larger declared
/// values are clamped to this. Well above any real page's needs (a
/// 1024px glyph is taller than most screens), and small enough that
/// rasterizing one glyph (`px²` bytes) stays ~1 MiB — see the
/// `font-size` arm in `declarations.rs`, and `text::MAX_FONT_PX`, the
/// same bound re-enforced independently at the rasterizer.
pub const MAX_FONT_SIZE_PX: f32 = 1024.0;

/// A CSS length-percentage: `px + percent%` (Milestone 33). Every unit
/// but `%` is resolved to pixels by the cascade (see `crate::length`);
/// the percentage waits for layout, which knows the containing block.
/// `calc()` over lengths is always such a sum, so it is exact.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Length {
    pub px: f32,
    /// In percent (`50%` is `50.0`).
    pub percent: f32,
}

impl Length {
    pub const ZERO: Length = Length { px: 0.0, percent: 0.0 };

    pub const fn px(px: f32) -> Length {
        Length { px, percent: 0.0 }
    }

    pub const fn percent(percent: f32) -> Length {
        Length { px: 0.0, percent }
    }

    /// The used value, against a containing-block dimension of `basis`
    /// pixels.
    pub fn resolve(self, basis: f32) -> f32 {
        self.px + self.percent * basis / 100.0
    }

    /// Whether the value depends on the containing block.
    pub fn has_percent(self) -> bool {
        self.percent != 0.0
    }

    /// Pixels only, when there is no percentage — for contexts where a
    /// percentage has nothing to resolve against (intrinsic sizing).
    pub fn absolute(self) -> Option<f32> {
        (!self.has_percent()).then_some(self.px)
    }
}

/// The four edges of a box-model layer. `Edges<f32>` ([`EdgeSizes`]) is
/// a used value in pixels; `Edges<Length>` (padding) and
/// `Edges<Option<Length>>` (margin, `None` = `auto`) are computed values.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

/// Milestone 6: resolved pixel edges — what layout works with.
pub type EdgeSizes = Edges<f32>;

impl<T: Copy> Edges<T> {
    pub fn all(v: T) -> Self {
        Edges { top: v, right: v, bottom: v, left: v }
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> Edges<U> {
        Edges { top: f(self.top), right: f(self.right), bottom: f(self.bottom), left: f(self.left) }
    }
}

impl Edges<Length> {
    /// Used values: percentages (padding and margin both) resolve against
    /// the containing block's WIDTH, on every side (CSS 2 §8.3, §8.4).
    pub fn resolve(self, containing_width: f32) -> EdgeSizes {
        self.map(|l| l.resolve(containing_width))
    }
}

impl Edges<Option<Length>> {
    /// Used values with `auto` as zero — the caller handles `auto`
    /// horizontal margins itself (centring).
    pub fn resolve(self, containing_width: f32) -> EdgeSizes {
        self.map(|l| l.map_or(0.0, |l| l.resolve(containing_width)))
    }
}

impl EdgeSizes {
    pub fn uniform(v: f32) -> Self {
        Edges::all(v)
    }
}

/// Border styles. `None` (or unset) means no border renders at all —
/// matches real CSS, where `border-width` alone has no visible effect
/// without `border-style` also being set.
///
/// Phase C adds `Dashed`/`Dotted`/`Double` — all painted by `rendering`
/// out of the same solid-quad primitive the engine already has
/// (segmented strips for dashed/dotted, two thin frames for double),
/// so no new GPU pipeline is needed. `Groove`/`Ridge`/`Inset`/`Outset`
/// (the 3D-looking styles) are still out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Solid,
    Dashed,
    Dotted,
    Double,
}

/// One side of an element's border (M6's gap: each side has its own).
/// A side whose `style` is `None` has no border, whatever its `width`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderSide {
    pub width: f32,
    pub style: BorderStyle,
    pub color: Color,
}

impl Default for BorderSide {
    fn default() -> Self {
        BorderSide { width: 0.0, style: BorderStyle::None, color: Color { r: 0, g: 0, b: 0, a: 255 } }
    }
}

/// A side as declared (M6's gap): its colour `None` until one is set —
/// `currentcolor`, read as the element's colour ([`crate::Style::border`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderSpec {
    pub width: f32,
    pub style: BorderStyle,
    pub color: Option<Color>,
}

impl BorderSide {
    /// The width layout uses: none without a style (CSS 2 §8.5.3).
    #[inline]
    pub fn used_width(&self) -> f32 {
        if self.style == BorderStyle::None { 0.0 } else { self.width.max(0.0) }
    }
}

impl Edges<BorderSide> {
    /// The same border on every side.
    pub fn uniform(width: f32, style: BorderStyle, color: Color) -> Self {
        Edges::all(BorderSide { width, style, color })
    }

    /// Whether no side has a border.
    pub fn is_none(&self) -> bool {
        [self.top, self.right, self.bottom, self.left].iter().all(|s| s.used_width() == 0.0)
    }

    /// The sides as layout uses them: a side without a style has width 0.
    pub fn used(&self) -> Self {
        self.map(|s| BorderSide { width: s.used_width(), ..s })
    }

    /// Each side's used width.
    #[inline]
    pub fn used_widths(&self) -> EdgeSizes {
        self.map(|s| s.used_width())
    }

    /// Sets every side's width, style or colour (the `border`,
    /// `border-width` … forms that name no side).
    pub fn set_all(&mut self, f: impl Fn(&mut BorderSide)) {
        for side in [&mut self.top, &mut self.right, &mut self.bottom, &mut self.left] {
            f(side);
        }
    }
}

impl EdgeSizes {
    /// Left plus right.
    #[inline]
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    /// Top plus bottom.
    #[inline]
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

/// Milestone 14: `transform: translate()/translateX()/translateY()/scale()`
/// — no 3D transforms, no `matrix()`, and (see `rendering` crate)
/// transform does NOT propagate to descendant boxes — a real,
/// documented scope reduction, not full CSS transform semantics.
///
/// Milestone 22: `rotate()`/`skew()` are now parsed into `rotate_deg`/
/// `skew_x_deg`/`skew_y_deg` — real values, correctly cascaded,
/// correctly interpolated by `apply_transitions` (a `transition:
/// transform` that changes the rotation angle genuinely animates the
/// NUMBER frame by frame) — but NOT yet applied at paint time.
/// `rendering::apply_transform_to_items` only ever repositions/resizes
/// an axis-aligned `DisplayItem::Rect` (translate + scale); a rotated
/// or sheared box isn't representable that way at all — real support
/// needs the GPU quad pipeline itself to carry a rotation/shear matrix
/// per instance (a `gpu_backend`/`quad.wgsl` change, not a `style`-side
/// one), which is real, separate, riskier work deliberately not
/// undertaken in the same pass as the interpolation engine below. The
/// same "parsed but inert" pattern this codebase already uses for
/// `:hover`/`:focus` (see `style::matches_compound`'s `Interactive`
/// arm) — not a silent gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub translate_x: f32,
    pub translate_y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub rotate_deg: f32,
    pub skew_x_deg: f32,
    pub skew_y_deg: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            translate_x: 0.0,
            translate_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotate_deg: 0.0,
            skew_x_deg: 0.0,
            skew_y_deg: 0.0,
        }
    }
}

/// `display` (Milestone 10: `block`/`flex`; Milestone 33: `none`,
/// `inline`, `inline-block`, `inline-flex`, `list-item`). Every element
/// starts from its tag's user-agent default ([`initial_display`]), which
/// any declaration overrides — so layout decides block vs. inline from
/// this alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Display {
    /// No box at all, for the element or its descendants.
    None,
    #[default]
    Block,
    /// Participates in its parent's line boxes; its content flows.
    Inline,
    /// An atomic box on its parent's line, laid out inside as a block.
    InlineBlock,
    Flex,
    /// An atomic box on its parent's line, laid out inside as flex.
    InlineFlex,
    /// Milestone 33: table layout (CSS 2 §17). A table part outside a
    /// table is laid out as a block (no anonymous table is built round it).
    Table,
    InlineTable,
    TableRowGroup,
    TableRow,
    TableCell,
    TableCaption,
    /// `table-column` / `-group`: no box.
    TableColumn,
    /// Milestone 33: grid layout (CSS Grid 1).
    Grid,
    InlineGrid,
}

impl Display {
    /// Whether the element sits on its parent's line as one atomic box.
    pub fn is_atomic_inline(self) -> bool {
        matches!(self, Display::InlineBlock | Display::InlineFlex | Display::InlineTable | Display::InlineGrid)
    }

    /// Whether the element's own content is laid out as a grid.
    pub fn is_grid(self) -> bool {
        matches!(self, Display::Grid | Display::InlineGrid)
    }

    /// Whether the element's own content is laid out as a table.
    pub fn is_table(self) -> bool {
        matches!(self, Display::Table | Display::InlineTable)
    }

    /// Whether the element's own content is laid out as a flex container.
    pub fn is_flex_container(self) -> bool {
        matches!(self, Display::Flex | Display::InlineFlex)
    }
}

/// An element's `display` before any declaration applies: the display
/// rules of the HTML Standard's user-agent stylesheet (§15.3), built into
/// the cascade so every styling path — with or without the UA sheet —
/// agrees on which elements are inline, atomic, or not rendered. Elements
/// the HTML Standard renders as `inline` replaced content (`img`, form
/// controls, `video`, …) are atomic inline boxes here.
pub fn initial_display(tag: &str) -> Display {
    match tag {
        "head" | "script" | "style" | "meta" | "link" | "title" | "base" | "template" | "noscript" | "area"
        | "datalist" | "param" | "source" | "track" | "rp" => Display::None,
        "span" | "a" | "b" | "i" | "em" | "strong" | "small" | "big" | "code" | "sub" | "sup" | "u" | "s"
        | "strike" | "del" | "ins" | "mark" | "abbr" | "acronym" | "cite" | "q" | "kbd" | "samp" | "var"
        | "tt" | "dfn" | "time" | "data" | "bdi" | "bdo" | "font" | "label" | "br" | "wbr" | "nobr"
        | "output" | "ruby" | "rt" | "slot" | "map" => Display::Inline,
        "img" | "input" | "button" | "select" | "textarea" | "video" | "audio" | "canvas" | "iframe"
        | "embed" | "object" | "svg" | "meter" | "progress" => Display::InlineBlock,
        "table" => Display::Table,
        "thead" | "tbody" | "tfoot" => Display::TableRowGroup,
        "tr" => Display::TableRow,
        "td" | "th" => Display::TableCell,
        "caption" => Display::TableCaption,
        "col" | "colgroup" => Display::TableColumn,
        _ => Display::Block,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum JustifyContent {
    #[default]
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AlignItems {
    FlexStart,
    Center,
    FlexEnd,
    #[default]
    Stretch,
}

/// Phase D: `flex-wrap`. `WrapReverse` wraps but stacks the lines in
/// the opposite cross-axis order.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FlexWrap {
    #[default]
    Nowrap,
    Wrap,
    WrapReverse,
}

/// Phase D: `align-content` — distributes flex LINES along the cross
/// axis (only meaningful when `flex-wrap` produced more than one line
/// and the container has spare cross-axis space).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AlignContent {
    #[default]
    Stretch,
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
    SpaceAround,
}

/// Phase D: intrinsic-sizing keyword for `width`/`height`
/// (`min-content` / `max-content` / `fit-content`). `None` on a
/// `Style` means the length/auto path applies as before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntrinsicKeyword {
    MinContent,
    MaxContent,
    FitContent,
}

/// Phase E: `object-fit` for a replaced element (`<img>`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ObjectFit {
    /// Stretch to fill the box, ignoring aspect ratio (CSS initial).
    #[default]
    Fill,
    /// Scale to fit inside the box, preserving aspect ratio (letterbox).
    Contain,
    /// Scale to cover the box, preserving aspect ratio (crop).
    Cover,
    /// Natural size, centred (may overflow or leave gaps).
    None,
    /// `min(None, Contain)` — never upscales.
    ScaleDown,
}

/// Phase E: `background-size`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BackgroundSize {
    /// Natural image size (CSS initial).
    #[default]
    Auto,
    Contain,
    Cover,
    /// Explicit sizes (Milestone 33: lengths or percentages of the box);
    /// `None` on an axis is `auto` (keeps the image's ratio).
    Explicit(Option<Length>, Option<Length>),
}

/// `background-position`: per axis, a pixel offset plus a percentage of
/// the FREE space (box minus image) — CSS's definition, so `50%` centres
/// and `100%` aligns right/bottom. Keywords map to 0/50/100%; `right 10px`
/// is `100% - 10px`. Initial value `0% 0%` (top-left).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BackgroundPosition {
    pub x: Length,
    pub y: Length,
}

/// Milestone 33: one `box-shadow` (pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    /// Inset shadows are parsed but not drawn yet.
    pub inset: bool,
}

/// Most shadows kept from one `box-shadow` list.
pub const MAX_BOX_SHADOWS: usize = 16;
/// Largest blur or spread radius honoured (px).
pub const MAX_SHADOW_EXTENT: f32 = 1024.0;

/// Milestone 33: `float`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// Milestone 33: `clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clear {
    pub left: bool,
    pub right: bool,
}

/// Milestone 33: `border-collapse` / `border-spacing` (inherited).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableBorders {
    pub collapse: bool,
    /// Horizontal and vertical spacing between cells (px).
    pub spacing: (f32, f32),
}

/// Milestone 33: `visibility` (inherited).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    /// Not painted, not hit — but still takes its space; a descendant can
    /// be `visible` again.
    Hidden,
}

/// Milestone 33: `overflow-x` / `overflow-y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    #[default]
    Visible,
    /// Clipped; scrollable only by script (here: not at all).
    Hidden,
    /// Clipped, never scrollable.
    Clip,
    /// Clipped, and the user can scroll it.
    Scroll,
    Auto,
}

impl Overflow {
    /// Whether content is clipped to the padding box on this axis.
    pub fn clips(self) -> bool {
        self != Overflow::Visible
    }

    /// Whether the user can scroll the content on this axis.
    pub fn user_scrollable(self) -> bool {
        matches!(self, Overflow::Scroll | Overflow::Auto)
    }
}

/// Milestone 33: `position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    /// In flow, then offset by `top`/`left`/`bottom`/`right`.
    Relative,
    /// Out of flow, placed in its containing block (the nearest positioned
    /// ancestor's padding box, else the initial containing block).
    Absolute,
    /// Out of flow, placed in the viewport; does not scroll.
    Fixed,
    /// In flow, then kept within the viewport by its insets while its
    /// parent is on screen.
    Sticky,
}

impl Position {
    /// Whether this is a containing block for absolute descendants.
    pub fn is_positioned(self) -> bool {
        self != Position::Static
    }

    /// Whether the box is taken out of normal flow.
    pub fn is_out_of_flow(self) -> bool {
        matches!(self, Position::Absolute | Position::Fixed)
    }
}

/// Milestone 33: `background-repeat`. `space` and `round` are treated as
/// `repeat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundRepeat {
    #[default]
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

impl BackgroundRepeat {
    pub fn repeats_x(self) -> bool {
        matches!(self, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatX)
    }

    pub fn repeats_y(self) -> bool {
        matches!(self, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatY)
    }
}
/// Inherited, same as `color`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum TextAlign {
    /// Milestone 35: the initial value — left in a left-to-right block,
    /// right in a right-to-left one.
    #[default]
    Start,
    End,
    Left,
    Center,
    Right,
    /// Phase B: last line stays left-aligned; earlier lines have their
    /// inter-word gaps stretched to fill the container width.
    Justify,
}

/// Phase B: `white-space`. Inherited. `PreLine` (collapse spaces but
/// keep newlines) is deferred — not in this phase's list.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum WhiteSpace {
    /// Collapse runs of whitespace, wrap at the container edge.
    #[default]
    Normal,
    /// Collapse whitespace, never wrap.
    Nowrap,
    /// Preserve every space and newline, never wrap.
    Pre,
    /// Preserve every space and newline, still wrap at the container edge.
    PreWrap,
}

/// Phase B: `vertical-align` keyword values. Not inherited (real CSS
/// doesn't inherit it either). `Sub`/`Super` shift by a fixed fraction
/// of the font size rather than a real subscript metric — a
/// documented simplification.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Top,
    Middle,
    Bottom,
    TextTop,
    TextBottom,
    Sub,
    Super,
}
