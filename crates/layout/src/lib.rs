//! Layout: block stacking, an inline formatting context (Milestone 5),
//! a complete box model (Milestone 6), and now (Milestone 7) images —
//! `<img>` as a replaced element sized from its decoded intrinsic
//! dimensions, and `background-image` painted behind an element's
//! normal content.
//!
//! Both cases funnel through one `LayoutBox.image` field — an `<img>`
//! and a `background-image` are rendered identically (an image drawn
//! at the box's position/size); what differs is only how each affects
//! *sizing* (an `<img>`'s auto size comes from the image's intrinsic
//! dimensions; a `background-image` never affects box sizing at all,
//! matching real CSS).
//!
//! ## Module map
//!
//! This crate root holds only the shared `LayoutBox`/`Border`/
//! `Outline`/`TextBox` types, the two public entry points, and the few
//! constants/helpers every submodule below needs:
//!
//! - [`box_model`] — the core per-element box-model computation
//!   (`layout_element`): margin/border/padding/content sizing, images,
//!   form-control defaults, and dispatch into block vs. flex layout.
//! - [`flex`] — `display: flex` container layout (row + column).
//! - [`inline`] — the inline formatting context: word tokenization,
//!   line-breaking, `text-align`/`vertical-align`.
//! - [`intrinsic`] — Phase D min-content/max-content measurement.
//! - [`hit_test`] — mapping a click position to a DOM element.

use css_parser::Stylesheet;
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use std::sync::Arc;
use style::{BorderStyle, Color, StyleCache, Transform};
use text::Font;

mod box_model;
mod flex;
mod floats;
mod grid;
mod hit_test;
mod inline;
mod intrinsic;
mod patch;
mod positioned;
mod reuse;
mod table;

pub use hit_test::{hit_test_counted, hit_test_path, hit_test_path_scrolled, Hit};
pub use intrinsic::{IntrinsicCache, IntrinsicSizes};
pub use reuse::LayoutReuse;
pub use dom::NodeId;

/// Milestone 33: how far each scroll container (by element) is scrolled,
/// `(x, y)` — paint-time state the host keeps; the layout never changes
/// for a scroll.
pub type ScrollOffsets = std::collections::HashMap<NodeId, (f32, f32)>;


#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    /// Top-left of the BORDER box.
    pub x: f32,
    pub y: f32,
    /// BORDER box dimensions (includes border + padding + content).
    pub width: f32,
    pub height: f32,
    pub background_color: Color,
    /// `None` when no side has a border. Boxed (M6's gap): four sides are
    /// 48 bytes, and most of a page's boxes have none.
    pub border: Option<Box<Border>>,
    /// Phase C: uniform `border-radius` in px (paint-time only — layout
    /// still treats the box as a rectangle). `0.0` = square corners.
    pub border_radius: f32,
    /// Milestone 33: whether this box clips its descendants to its padding
    /// box (`overflow` other than `visible`), per axis, and whether the
    /// user can scroll it.
    pub overflow: OverflowClip,
    /// Milestone 33: `opacity` — multiplies the alpha of everything this
    /// box and its descendants paint.
    pub opacity: f32,
    /// Milestone 33: `visibility` — a hidden box paints nothing of its own
    /// and is not hit, but its space and its visible descendants remain.
    pub visible: bool,
    /// Milestone 7: either this box's `<img src="...">` content, or a
    /// `background-image` — see module docs for why both funnel
    /// through one field.
    pub image: Option<Arc<images::DecodedImage>>,
    /// Milestone 33: the paint properties most boxes do not have (outline,
    /// image placement, positioning, shadows, transform), out of line —
    /// every word of text is a `LayoutBox`, and the whole tree is walked on
    /// every painted frame. `None` is all of them at their defaults; read
    /// through [`LayoutBox::extras`].
    pub extras: Option<Box<BoxExtras>>,
    /// Milestone 15: `dom::element_ptr_key` of the source element,
    /// for hit-testing/event dispatch to map a click position back to
    /// a real DOM element. For a word box (inline text), this is the
    /// innermost INLINE element the word came from (`<a>`, `<b>`,
    /// `<span>`, ...) if any — see `InlineWord::element_key`'s doc
    /// comment. `None` for plain text with no inline wrapper (see
    /// `hit_test_path`'s doc comment for how that case still resolves
    /// to the right target via bubbling to the nearest ancestor that
    /// DOES have a key).
    pub element_key: Option<dom::NodeId>,
    pub text: Option<TextBox>,
    pub children: Vec<LayoutBox>,
    /// Milestone 39b (ADR 022): the frame this box's children are in, and
    /// what layout records about the subtree for hit testing and reuse.
    pub subtree: Subtree,
}

/// Milestone 39b (ADR 022): a box's position is in its parent's frame;
/// `child_offset` shifts the frame its children are in. A fresh layout
/// leaves every offset at zero — positions are document positions — and a
/// subtree moved over from the previous frame is moved in O(1) by its
/// root's position and offset.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Subtree {
    /// Added to every descendant's position (and baseline).
    pub child_offset: (f32, f32),
    /// Where this box or a descendant can be hit, `(x0, y0, x1, y1)` in the
    /// parent's frame (an axis may be unbounded); `None` until layout's
    /// final pass computes it — and then nothing is pruned.
    pub bounds: Option<(f32, f32, f32, f32)>,
    /// The children's bounds are in increasing order of both vertical
    /// edges and none is positioned: hit testing may binary-search them.
    pub ordered: bool,
    /// Boxes in the subtree, this one included (0 until computed).
    pub boxes: u32,
    /// The layout frame that built this box or last changed it
    /// (`LayoutReuse` numbers frames; 0 outside incremental layout). A box
    /// from an earlier frame than its tree's root is unchanged since then
    /// but for where it is: a page's display list may move its items over
    /// instead of building them (Milestone 39c: a stamp, not a flag, so a
    /// box kept by a patch counts too).
    pub frame: u32,
}

/// Milestone 39b: a box and the origin of the frame it is in — what a
/// reader below the top of the tree needs to place it.
#[derive(Debug, Clone, Copy)]
pub struct Placed<'a> {
    pub layout: &'a LayoutBox,
    pub origin: (f32, f32),
}

impl<'a> Placed<'a> {
    /// The tree's root, whose frame is the document.
    pub fn root(layout: &'a LayoutBox) -> Self {
        Placed { layout, origin: (0.0, 0.0) }
    }

    /// The box's border-box origin in document space.
    pub fn x(&self) -> f32 {
        self.origin.0 + self.layout.x
    }

    pub fn y(&self) -> f32 {
        self.origin.1 + self.layout.y
    }

    /// The frame this box's children are in.
    pub fn child_origin(&self) -> (f32, f32) {
        let (ox, oy) = self.layout.subtree.child_offset;
        (self.origin.0 + ox, self.origin.1 + oy)
    }

    /// The children, placed.
    pub fn children(self) -> impl DoubleEndedIterator<Item = Placed<'a>> + 'a {
        let origin = self.child_origin();
        self.layout.children.iter().map(move |layout| Placed { layout, origin })
    }

    /// A copy of the box (without its children) in document space — for
    /// code that reads one box's geometry.
    pub fn to_document_space(&self) -> LayoutBox {
        let mut b = self.layout.clone_shallow();
        let (ox, oy) = self.origin;
        b.x += ox;
        b.y += oy;
        if let Some(t) = &mut b.text {
            t.baseline_y += oy;
        }
        if let Some((x0, y0, x1, y1)) = &mut b.subtree.bounds {
            (*x0, *y0, *x1, *y1) = (*x0 + ox, *y0 + oy, *x1 + ox, *y1 + oy);
        }
        b
    }

    /// A copy of the box and its children (without theirs) in document
    /// space — for code that reads a box and its line's words (the caret,
    /// the focus ring, click-to-position).
    pub fn with_children_in_document_space(&self) -> LayoutBox {
        let mut b = self.to_document_space();
        b.subtree.child_offset = (0.0, 0.0);
        b.children = self.children().map(|c| c.to_document_space()).collect();
        b
    }
}

/// Milestone 33: a [`LayoutBox`]'s rarely-set paint properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoxExtras {
    /// Phase C: `Some` when `outline-style` isn't `none`.
    pub outline: Option<Outline>,
    /// Phase E: how `image` is sized/placed within the box.
    /// `image_is_content` distinguishes an `<img>`'s own image (uses
    /// `object_fit`) from a `background-image` (uses `background_size` /
    /// `background_position` / `background_repeat`).
    pub image_is_content: bool,
    pub object_fit: style::ObjectFit,
    pub background_size: style::BackgroundSize,
    pub background_position: style::BackgroundPosition,
    pub background_repeat: style::BackgroundRepeat,
    /// Milestone 33: `position` / `z-index` — paint order, scrolling
    /// (`fixed`, `sticky`) and hit testing.
    pub positioning: Positioning,
    /// Milestone 33: `box-shadow`s, first on top.
    pub box_shadow: Box<[style::BoxShadow]>,
    /// Milestone 14: `transform: translate()/scale()`, applied at paint
    /// time by `rendering`.
    pub transform: Transform,
    /// Milestone 34: painted on a transform layer of its own even while the
    /// transform is the identity — it may animate (see
    /// `style::Style::composited_transform`).
    pub promoted: bool,
}

impl LayoutBox {
    /// The box's rarely-set paint properties (defaults when none are set).
    pub fn extras(&self) -> &BoxExtras {
        static DEFAULT: std::sync::OnceLock<BoxExtras> = std::sync::OnceLock::new();
        self.extras.as_deref().unwrap_or_else(|| DEFAULT.get_or_init(BoxExtras::default))
    }

    /// `position` of this box — a hot check (paint order, scrolling, hit
    /// testing), so it does not go through [`LayoutBox::extras`]' default.
    pub fn position_scheme(&self) -> style::Position {
        self.extras.as_deref().map_or(style::Position::Static, |e| e.positioning.scheme)
    }

    /// Whether this box is positioned (see [`Positioning::is_positioned`]).
    pub fn is_positioned(&self) -> bool {
        self.position_scheme().is_positioned()
    }

    /// The box's rarely-set paint properties, to set one.
    pub fn extras_mut(&mut self) -> &mut BoxExtras {
        self.extras.get_or_insert_with(Default::default)
    }

    /// Milestone 39b: marks a box changed after it was built or moved over
    /// (anything but a translation): its subtree's bounds and box count are
    /// recomputed, and a display list builds it again.
    pub(crate) fn touch(&mut self) {
        // Its bounds are recomputed, and it is stamped with this frame.
        self.subtree.bounds = None;
    }

    /// This box without its children.
    fn clone_shallow(&self) -> LayoutBox {
        LayoutBox {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            background_color: self.background_color,
            border: self.border.clone(),
            border_radius: self.border_radius,
            overflow: self.overflow,
            opacity: self.opacity,
            visible: self.visible,
            image: self.image.clone(),
            extras: self.extras.clone(),
            element_key: self.element_key,
            text: self.text.clone(),
            children: Vec::new(),
            subtree: self.subtree,
        }
    }
}

/// A box's border, each side with its own used width, style and colour
/// (M6's gap). A side with no border has width 0; a box with none on any
/// side has `LayoutBox.border == None`.
pub type Border = style::Edges<style::BorderSide>;

/// Phase C: an `outline` — painted just outside the border box at
/// `offset` px, purely a paint concern (no layout effect).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outline {
    pub width: f32,
    pub color: Color,
    pub style: BorderStyle,
    pub offset: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextBox {
    pub content: String,
    pub font_size: f32,
    pub color: Color,
    pub baseline_y: f32,
    /// Milestone 35: how the word is shaped (font, size, direction) —
    /// the same spec layout measured it with, so paint draws what was
    /// measured.
    pub spec: text::TextSpec,
    /// Milestone 35: its text decoration lines.
    pub decoration: style::Decoration,
}

/// Milestone 33: a box's `overflow` behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OverflowClip {
    pub x: bool,
    pub y: bool,
    pub scrollable: bool,
}

impl OverflowClip {
    pub fn clips(&self) -> bool {
        self.x || self.y
    }
}

impl LayoutBox {
    /// The padding box (inside the border): where `overflow` clips, and
    /// what a scroll container scrolls. `(x, y, width, height)`.
    pub fn padding_box(&self) -> (f32, f32, f32, f32) {
        let b = self.border.as_deref().map_or_else(style::EdgeSizes::default, |b| b.used_widths());
        (self.x + b.left, self.y + b.top, (self.width - b.horizontal()).max(0.0), (self.height - b.vertical()).max(0.0))
    }

    /// The clip this box applies to its descendants, as `(x0, y0, x1, y1)`;
    /// an axis that does not clip is unbounded.
    pub fn clip_rect(&self) -> Option<(f32, f32, f32, f32)> {
        if !self.overflow.clips() {
            return None;
        }
        let (x, y, w, h) = self.padding_box();
        let (x0, x1) = if self.overflow.x { (x, x + w) } else { (f32::NEG_INFINITY, f32::INFINITY) };
        let (y0, y1) = if self.overflow.y { (y, y + h) } else { (f32::NEG_INFINITY, f32::INFINITY) };
        Some((x0, y0, x1, y1))
    }

    /// How far this scroll container's content extends past its padding
    /// box — the most it can be scrolled, `(x, y)`. Its in-flow and
    /// positioned descendants count (not `fixed` ones, which the viewport
    /// contains).
    pub fn scroll_range(&self) -> (f32, f32) {
        let (px, py, pw, ph) = self.padding_box();
        let (mut right, mut bottom) = (px + pw, py + ph);
        // Positions in this box's parent's frame (Milestone 39b).
        let mut stack: Vec<(&LayoutBox, (f32, f32))> = self.children.iter().map(|c| (c, self.subtree.child_offset)).collect();
        while let Some((b, (ox, oy))) = stack.pop() {
            if b.position_scheme() == style::Position::Fixed {
                continue;
            }
            right = right.max(ox + b.x + b.width);
            bottom = bottom.max(oy + b.y + b.height);
            // A clipping descendant's own overflow is its business.
            if !b.overflow.clips() {
                let origin = (ox + b.subtree.child_offset.0, oy + b.subtree.child_offset.1);
                stack.extend(b.children.iter().map(|c| (c, origin)));
            }
        }
        ((right - px - pw).max(0.0), (bottom - py - ph).max(0.0))
    }
}

/// Milestone 33: how a box is positioned.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Positioning {
    pub scheme: style::Position,
    pub z_index: Option<i32>,
    /// For `position: sticky`: what keeps it in view (boxed: rare, and
    /// every box — most of them words — carries this struct).
    pub sticky: Option<Box<Sticky>>,
}

/// A sticky box's constraints, fixed at layout time: its insets from the
/// viewport's top and bottom edges, and how far it may move within its
/// containing block (it never leaves it). Milestone 39b: relative to the
/// box, so they move with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sticky {
    pub top: Option<f32>,
    pub bottom: Option<f32>,
    /// From the containing block's content top down to the box's top.
    pub room_above: f32,
    /// From the box's bottom down to the containing block's content bottom.
    pub room_below: f32,
    pub viewport_height: f32,
}

impl Positioning {
    /// Whether this box is painted in the positioned layer (after in-flow
    /// content), CSS 2 Appendix E step 8-9.
    pub fn is_positioned(&self) -> bool {
        self.scheme.is_positioned()
    }

    /// The paint layer: negative `z-index` below in-flow content, `auto`
    /// and zero with it at 0, positive above.
    pub fn z(&self) -> i32 {
        self.z_index.unwrap_or(0)
    }
}

impl LayoutBox {
    /// How far down a sticky box is shifted when the page is scrolled by
    /// `scroll` px (0 for anything else): enough to keep its top `top` px
    /// below the viewport's top (or its bottom `bottom` px above the
    /// viewport's bottom), never out of its containing block.
    ///
    /// `frame_y` is the origin of the frame the box is in (Milestone 39b).
    pub fn sticky_offset(&self, frame_y: f32, scroll: f32) -> f32 {
        let Some(s) = self.extras().positioning.sticky.as_deref() else { return 0.0 };
        let top_in_view = (frame_y + self.y) - scroll;
        let mut shift = 0.0f32;
        if let Some(top) = s.top {
            if top_in_view < top {
                shift = top - top_in_view;
            }
        }
        if let Some(bottom) = s.bottom {
            let bottom_in_view = top_in_view + self.height;
            let limit = s.viewport_height - bottom;
            if bottom_in_view > limit {
                shift = limit - bottom_in_view;
            }
        }
        // Clamped to the containing block, in both directions.
        shift.min(s.room_below).max(-s.room_above)
    }
}

/// `children` in paint order (CSS 2 Appendix E, per box): positioned boxes
/// with a negative `z-index`, then in-flow boxes in tree order, then
/// positioned boxes with `z-index` `auto`/0 and positive, by `z-index`
/// (tree order within equal ones). Hit testing visits them in reverse.
///
/// Allocation-free in the common case (no positioned children): this runs
/// for every box of every painted frame.
#[inline]
pub fn paint_order(children: &[LayoutBox]) -> PaintOrder<'_> {
    if !children.iter().any(|c| c.is_positioned()) {
        return PaintOrder::TreeOrder(children.iter());
    }
    let mut below: Vec<&LayoutBox> = children.iter().filter(|c| c.extras().positioning.is_positioned() && c.extras().positioning.z() < 0).collect();
    below.sort_by_key(|c| c.extras().positioning.z());
    let mut above: Vec<&LayoutBox> = children.iter().filter(|c| c.extras().positioning.is_positioned() && c.extras().positioning.z() >= 0).collect();
    above.sort_by_key(|c| c.extras().positioning.z());
    PaintOrder::Sorted(below.into_iter().chain(children.iter().filter(|c| !c.extras().positioning.is_positioned())).chain(above).collect::<Vec<_>>().into_iter())
}

/// [`paint_order`]'s iterator (reversible: hit testing walks it backwards).
pub enum PaintOrder<'a> {
    TreeOrder(std::slice::Iter<'a, LayoutBox>),
    Sorted(std::vec::IntoIter<&'a LayoutBox>),
}

impl<'a> Iterator for PaintOrder<'a> {
    type Item = &'a LayoutBox;
    #[inline]
    fn next(&mut self) -> Option<&'a LayoutBox> {
        match self {
            PaintOrder::TreeOrder(i) => i.next(),
            PaintOrder::Sorted(i) => i.next(),
        }
    }
}

impl DoubleEndedIterator for PaintOrder<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            PaintOrder::TreeOrder(i) => i.next_back(),
            PaintOrder::Sorted(i) => i.next_back(),
        }
    }
}

// Milestone 33: which elements are inline, atomic or not rendered is their
// computed `display` (see `style::initial_display` for the per-tag
// defaults that used to be tag lists here). A `<script>`/`<style>` whose
// source text must never be laid out as page content is `display: none`.

/// Caps how deep `box_model::layout_element` (and the flex/intrinsic
/// helpers it calls into) will recurse, independent of how deep the
/// DOM actually is. `html_parser::MAX_NESTING_DEPTH` bounds
/// HTML-PARSED trees at 512, but a tree built/mutated via JS
/// `appendChild` isn't bounded there, and this recursion has no bound
/// of its own — a sufficiently deep tree overflows the native stack.
/// Past the cap, an element still gets its own real `LayoutBox` (its
/// own box-model math still runs normally); only descending into ITS
/// children is skipped, so a pathologically deep subtree just renders
/// as a childless box at that point instead of crashing.
pub(crate) const MAX_LAYOUT_DEPTH: usize = 512;

/// Milestone 33: the stack a thread needs to lay out, paint and hit-test a
/// document nested to [`MAX_LAYOUT_DEPTH`] with any mix of layout modes.
/// Layout is recursive; measured at that depth, block needs about 1.5 MiB,
/// flex about 2 MiB, grid about 3 MiB. This keeps well over 2x headroom;
/// `integration_tests/tests/stack_depth.rs` checks every mode fits in half
/// of it. The engine's page threads have more (a compile-time check there).
pub const MIN_STACK_BYTES: usize = 8 << 20;

/// Lays out the document tree starting at (0, 0), within the given
/// viewport width. `image_cache` is mutable — `<img>`/`background-image`
/// decode on first use and get cached by path for reuse across the
/// whole layout pass and across frames (caller owns the cache and
/// keeps it alive across calls, same pattern as `text::GlyphAtlas`).
///
/// Concurrency note: every element's style is resolved ONCE, up
/// front, in parallel across CPU cores, via
/// `style::resolve_style_tree_parallel` — see that function's doc
/// comment. This replaced an earlier design where `layout_element`
/// called `resolve_style` on demand during its own recursive walk
/// (sequentially, and in one specific path, redundantly — the same
/// element's style was computed twice). Layout itself stays
/// single-threaded (block positioning has genuine sequential
/// dependencies — each sibling's `y` depends on the previous
/// sibling's height — so it isn't embarrassingly parallel the way
/// style resolution is), but it now consumes already-parallelized
/// results instead of redoing cascade matching inline.
/// Concurrency work (post-Milestone-17 audit): `<img>`/`background-image`
/// decoding used to happen one at a time, inline, exactly when
/// `layout_element`'s tree walk reached each element — real,
/// unnecessary sequential cost for work with zero cross-image
/// dependency (see `images::ImageCache::preload_parallel`'s doc
/// comment). This pre-pass walks the tree ONCE (reusing the
/// already-computed `style_cache` — no extra cascade work) to collect
/// every needed image path, decodes the whole batch in parallel, and
/// only THEN runs the normal (still-sequential, correctly so — see
/// `layout_element`'s module doc comment on why positioning isn't
/// parallelized) layout walk, which now hits an already-warm cache.
pub fn layout_document<'a>(
    root: impl Into<ElementRef<'a>>,
    viewport_width: f32,
    font: &Font,
    stylesheet: &Stylesheet,
    image_cache: &mut ImageCache,
) -> LayoutBox {
    let root = root.into();
    let style_cache = style::resolve_style_tree_parallel(root, stylesheet, viewport_width);
    layout_document_with_style_cache(root, viewport_width, font, &style_cache, image_cache)
}

/// Milestone 22: everything `layout_document` does AFTER cascade
/// resolution, taking an already-resolved `StyleCache` instead of
/// resolving one itself — the hook `app` uses to lay out a frame whose
/// styles have been overwritten by `style::apply_transitions`/
/// `apply_animations` (those need to run BETWEEN cascade and layout,
/// which `layout_document`'s all-in-one signature had no seam for).
/// Purely additive: `layout_document` is now a thin wrapper around this,
/// so every existing caller/test keeps working unchanged.
pub fn layout_document_with_style_cache<'a>(
    root: impl Into<ElementRef<'a>>,
    viewport_width: f32,
    font: &Font,
    style_cache: &StyleCache,
    image_cache: &mut ImageCache,
) -> LayoutBox {
    let root = root.into();
    let mut image_paths = Vec::new();
    intrinsic::collect_needed_image_paths(root, style_cache, &mut image_paths, 0);
    image_cache.preload_parallel(&image_paths);

    // Phase D: parallel intrinsic-size measurement. Runs only if the
    // tree actually needs it (a `min/max/fit-content` keyword or a flex
    // container anywhere) — otherwise an empty cache, consumed by
    // nothing. See `intrinsic::measure_intrinsic_parallel`'s doc comment.
    let intrinsic = if intrinsic::tree_needs_intrinsic_sizing(root, style_cache, 0) {
        intrinsic::measure_intrinsic_parallel(root, style_cache, font, image_cache)
    } else {
        IntrinsicCache::new()
    };

    let mut reuse = LayoutReuse::new();
    reuse.set_viewport(viewport_width, style::DEFAULT_VIEWPORT_HEIGHT);
    let mut laid = box_model::layout_element(root, 0.0, 0.0, viewport_width, font, style_cache, &intrinsic, image_cache, &mut reuse, 0);
    place_viewport_out_of_flow(root, &mut laid, font, style_cache, &intrinsic, image_cache, &mut reuse);
    reuse::finish_subtrees(&mut laid, reuse.frame());
    laid
}

/// Milestone 33: the out-of-flow boxes the viewport contains — `fixed`
/// ones, and `absolute` ones with no positioned ancestor (the initial
/// containing block) — laid out and hoisted into the root's box. Only
/// when the root was laid out this frame: a reused root box already holds
/// them.
fn place_viewport_out_of_flow(
    root: ElementRef<'_>,
    laid: &mut LayoutBox,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut LayoutReuse,
) {
    if reuse.root_was_taken(root.node_id()) {
        return;
    }
    let (width, height) = reuse.viewport;
    let icb = positioned::ContainingBlock { x: 0.0, y: 0.0, width, height };
    let root_is_positioned = style_cache.get(&root.node_id()).is_some_and(|s| s.position.is_positioned());
    let kinds: &[positioned::Collect] =
        if root_is_positioned { &[positioned::Collect::Fixed] } else { &[positioned::Collect::Absolute, positioned::Collect::Fixed] };
    positioned::place_descendants(root, laid, icb, kinds, font, style_cache, intrinsic, image_cache, reuse, 0);
}

/// Milestone 31: [`layout_document_with_style_cache`], reusing every
/// subtree of the previous frame's layout that `reuse` allows (see
/// `LayoutReuse::begin_frame`, which the caller must call first with the
/// previous tree and the layout-dirty elements). The result is identical to
/// a fresh layout — an oracle test checks exactly that — but a change deep
/// in a page re-lays out one ancestor chain instead of the page.
///
/// `changed` lists the elements that were mutated or restyled this frame;
/// when a previous layout is being reused, only their subtrees are searched
/// for images to decode (every image elsewhere was decoded when that part of
/// the page was first laid out).
pub fn layout_document_incremental<'a>(
    root: impl Into<ElementRef<'a>>,
    viewport_width: f32,
    font: &Font,
    style_cache: &StyleCache,
    image_cache: &mut ImageCache,
    reuse: &mut LayoutReuse,
    changed: &[dom::NodeId],
) -> LayoutBox {
    let root = root.into();
    let intrinsic = {
        let _s = diagnostics::tracing::info_span!("layout_prepass").entered();
        let mut image_paths = Vec::new();
        let incremental = reuse.has_previous();
        let needed = if incremental {
            // Only the changed subtrees can hold new images or newly need
            // intrinsic sizing; they are reached through the dirty set (the
            // changed elements' ancestors), not by walking the page.
            let changed: dom::NodeIdSet = changed.iter().copied().collect();
            let mut needed = reuse.intrinsic_needed;
            for_changed_subtrees(root, reuse.dirty(), &changed, |el, depth| {
                intrinsic::collect_needed_image_paths(el, style_cache, &mut image_paths, depth);
                needed = needed || intrinsic::tree_needs_intrinsic_sizing(el, style_cache, depth);
            });
            needed
        } else {
            intrinsic::collect_needed_image_paths(root, style_cache, &mut image_paths, 0);
            intrinsic::tree_needs_intrinsic_sizing(root, style_cache, 0)
        };
        image_cache.preload_parallel(&image_paths);
        reuse.intrinsic_needed = needed;
        if needed {
            let mut previous = std::mem::take(&mut reuse.intrinsic);
            let dirty = incremental.then(|| reuse.dirty().clone());
            let measured = std::sync::atomic::AtomicUsize::new(0);
            // Milestone 39c: when one element holds every change and still
            // contributes to its parent what it did, its ancestors' sizes
            // stand — only its subtree is measured.
            let target = if incremental { patch::measured_root(root, style_cache, reuse, changed) } else { None };
            let mut done = false;
            let mut dirty = dirty;
            if let (Some((target, depth)), Some(set)) = (target, dirty.as_mut()) {
                let (sizes, unchanged) = intrinsic::measure_subtree(target, depth, style_cache, font, image_cache, previous, set, &measured);
                previous = sizes;
                done = unchanged;
                // Otherwise its ancestors are measured — not it again.
                let document = root.document();
                set.retain(|&n| {
                    let el = document.and_then(|d| d.find_by_node_id(style::owner_of(n)));
                    !el.is_some_and(|el| std::iter::successors(Some(el), |e| e.parent()).any(|e| e.node_id() == target.node_id()))
                });
            }
            let sizes = if done { previous } else { intrinsic::measure_intrinsic(root, style_cache, font, image_cache, previous, dirty.as_ref(), &measured) };
            reuse.intrinsic_measured = measured.into_inner();
            sizes
        } else {
            IntrinsicCache::new()
        }
    };
    // The viewport's width is this call's; its height is the caller's
    // (`LayoutReuse::set_viewport`).
    reuse.viewport.0 = viewport_width;
    let laid_out = {
        let _s = diagnostics::tracing::info_span!("layout_boxes").entered();
        // Milestone 39c: a change that keeps its element's size is patched
        // into the previous layout in place (ADR 022 §8).
        match patch::try_patch(root, font, style_cache, &intrinsic, image_cache, reuse, changed) {
            Some(patched) => patched,
            None => {
                let mut laid = box_model::layout_element(root, 0.0, 0.0, viewport_width, font, style_cache, &intrinsic, image_cache, reuse, 0);
                place_viewport_out_of_flow(root, &mut laid, font, style_cache, &intrinsic, image_cache, reuse);
                reuse::finish_subtrees(&mut laid, reuse.frame());
                laid
            }
        }
    };
    // (A patched root keeps its link, written with its layout.)
    if reuse.patched().is_none() {
        reuse.link(root.node_id(), None, 0);
    }
    {
        let _s = diagnostics::tracing::info_span!("layout_release").entered();
        reuse.intrinsic = intrinsic;
        reuse.end_frame();
    }
    laid_out
}

/// Calls `visit` with each topmost `changed` element (and its depth), found
/// by descending only through `dirty` elements — the changed elements'
/// ancestors — so the walk costs O(dirty), not O(page).
fn for_changed_subtrees(
    root: ElementRef<'_>,
    dirty: &dom::NodeIdSet,
    changed: &dom::NodeIdSet,
    mut visit: impl FnMut(ElementRef<'_>, usize),
) {
    if changed.is_empty() {
        return;
    }
    let mut stack = vec![(root, 0usize)];
    while let Some((el, depth)) = stack.pop() {
        if changed.contains(&el.node_id()) {
            visit(el, depth);
            continue;
        }
        if depth >= MAX_LAYOUT_DEPTH || !dirty.contains(&el.node_id()) {
            continue;
        }
        for child in el.children() {
            if let ChildRef::Element(c) = child {
                stack.push((c, depth + 1));
            }
        }
    }
}

impl LayoutBox {
    /// A zero-sized, childless placeholder (what a reused subtree leaves
    /// behind in the previous frame's tree, which is then discarded).
    pub fn empty() -> LayoutBox {
        LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: crate::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: Vec::new(),
            subtree: Subtree::default(),
        }
    }
}
