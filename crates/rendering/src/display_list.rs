//! Paint stage: walks a `layout::LayoutBox` tree and emits a flat,
//! renderer-agnostic display list.
//!
//! `DisplayItem` is the seam that stays stable as later milestones add
//! variants — Milestone 2 added `Glyphs`, Milestone 7 adds `Image`.

use layout::{Border, LayoutBox};
use style::BorderStyle;
use std::collections::HashSet;
use std::sync::Arc;
use style::{Color, Transform};
use text::{Font, GlyphAtlas};

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayItem {
    Rect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        /// Phase C: uniform corner radius in px (`0.0` = square). Only
        /// the element's background fill carries a non-zero radius;
        /// border strips stay square-cornered for now (a stroked
        /// rounded-rect needs a dedicated shader path — documented
        /// scope cut). `gpu_backend`'s `quad.wgsl` rounds the corners
        /// with an SDF in the fragment shader.
        radius: f32,
    },
    /// One `Glyphs` item per text run (not one per character) — a
    /// text run paints as a single draw-adjacent unit. Each instance
    /// references a rect in the (external, GPU-owned) glyph atlas
    /// texture; `atlas_generation` lets a renderer detect when it
    /// needs to re-upload that texture before drawing these.
    Glyphs {
        instances: Vec<GlyphInstance>,
        atlas_generation: u64,
    },
    /// Milestone 7: an `<img>` or `background-image`, drawn at its
    /// box's position/size. `image_id` references an entry in the
    /// `Scene`'s `images` list — see `crate::scene::ImageResource`
    /// for why this is an id-indirection rather than embedding pixel
    /// data directly in every occurrence (the same image can appear
    /// in many display items; the pixel data should exist once).
    Image {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        image_id: u64,
        /// Milestone 33: which part of the image, at what opacity — `None`
        /// for the whole image, opaque (the common case, kept out of line so
        /// every display item stays 32 bytes). See [`ImageSampling`].
        sampling: Option<Box<ImageSampling>>,
    },
    /// Milestone 33: a `box-shadow` — a rounded rect (`x`..`height`, the
    /// shadow's shape) whose edge fades out over `blur` px on each side.
    Shadow {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
        radius: f32,
        blur: f32,
    },
    /// Milestone 34: everything up to the matching `PopClip` is clipped to
    /// `rect` (`[x, y, w, h]`) with corners rounded by `radius` — in the
    /// coordinate space current here, so a clip inside a rotated subtree
    /// rotates with it. Clips nest (they intersect).
    PushClip { rect: [f32; 4], radius: f32 },
    PopClip,
    /// Milestone 34: everything up to the matching `PopTransform` is drawn
    /// through the layer's matrix, composed with the transforms around it.
    PushTransform(Box<TransformLayer>),
    PopTransform,
    /// Milestone 34: group opacity — everything up to the matching
    /// `PopEffect` is drawn as one layer, and the layer at `opacity` (so
    /// overlapping descendants do not show through each other). `bounds`
    /// (`[x, y, w, h]`) is what the group covers.
    PushEffect { opacity: f32, bounds: [f32; 4] },
    PopEffect,
}

/// Milestone 34: what a `PushTransform` carries.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformLayer {
    /// Maps the coordinates of the content (document space) to where it is
    /// drawn, before the transforms around it.
    pub matrix: crate::Affine,
    /// The bounding box `[x, y, w, h]` of the content once transformed (in
    /// the space around this layer): culling and damage use it.
    pub bounds: [f32; 4],
    /// The element whose `transform` this is — so a frame that only changes
    /// that transform (a compositor animation) can update this layer in
    /// place — and its pivot, the box centre.
    pub element: Option<u64>,
    pub origin: [f32; 2],
    /// `Some(clip)` for a scroll container's layer: the matrix is the
    /// container's scroll offset (a translation), so scrolling it is a
    /// compositor update; `clip` (`[x, y, w, h]`, the container's padding
    /// box) bounds what the layer can show — its bounds never extend past
    /// it, so a long scrolled list off screen costs nothing.
    pub scroll: Option<[f32; 4]>,
}

impl TransformLayer {
    /// The layer's bounds for `content` (in its own space): mapped by the
    /// matrix, and limited to a scroll layer's clip.
    fn bounds_of(&self, content: Option<(f32, f32, f32, f32)>) -> [f32; 4] {
        let Some(b) = content else { return [0.0; 4] };
        let (x, y, w, h) = self.matrix.map_rect(b);
        match self.scroll {
            None => [x, y, w, h],
            Some([cx, cy, cw, ch]) => {
                let (x0, y0) = (x.max(cx), y.max(cy));
                let (x1, y1) = ((x + w).min(cx + cw), (y + h).min(cy + ch));
                [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
            }
        }
    }
}

impl DisplayItem {
    /// Whether this item opens or closes a clip, transform or effect.
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            DisplayItem::PushClip { .. }
                | DisplayItem::PopClip
                | DisplayItem::PushTransform(_)
                | DisplayItem::PopTransform
                | DisplayItem::PushEffect { .. }
                | DisplayItem::PopEffect
        )
    }
}

/// The whole image, once.
pub const FULL_UV: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// Milestone 33: how an image item samples its image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageSampling {
    /// The part of the image drawn into the rect, as normalised texture
    /// coordinates `[u0, v0, u1, v1]` — [`FULL_UV`] for the whole image.
    /// Coordinates outside 0..1 TILE the image (a repeating background is
    /// one item however many tiles it has); inside it they select a
    /// sub-rectangle (an image clipped to its box).
    pub uv: [f32; 4],
    /// Opacity (0..=1).
    pub alpha: f32,
}

impl ImageSampling {
    pub const WHOLE: ImageSampling = ImageSampling { uv: FULL_UV, alpha: 1.0 };

    /// An item's sampling (`None` is the whole image, opaque).
    pub fn of(sampling: &Option<Box<ImageSampling>>) -> ImageSampling {
        sampling.as_deref().copied().unwrap_or(ImageSampling::WHOLE)
    }

    /// The field value for `uv` at `alpha` (`None` when it is the default).
    pub fn boxed(uv: [f32; 4], alpha: f32) -> Option<Box<ImageSampling>> {
        let s = ImageSampling { uv, alpha };
        (s != ImageSampling::WHOLE).then(|| Box::new(s))
    }
}

/// Whether an image item's `uv` tiles (reaches outside the image).
pub fn uv_tiles(uv: [f32; 4]) -> bool {
    uv.iter().any(|c| *c < -1e-4 || *c > 1.0 + 1e-4)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphInstance {
    /// Top-left position of this glyph's quad, in layout-space pixels.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Pixel-space rect within the glyph atlas texture. NOT
    /// normalized — see `text::AtlasEntry` for why: normalize against
    /// the atlas's current dimensions at draw time.
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub color: Color,
}

/// Emits four solid-color `Rect`s forming a frame at `node`'s outer
/// edge — top and bottom strips span the full width; left and right
/// strips are inset vertically so the four pieces meet cleanly at the
/// corners without overlapping (overlap would matter if the border
/// color had partial alpha; clean joins cost nothing extra to get
/// right here).
fn rect(x: f32, y: f32, width: f32, height: f32, color: Color) -> DisplayItem {
    DisplayItem::Rect { x, y, width, height, color, radius: 0.0 }
}

/// Emits one edge of a border/outline frame as one or more solid
/// rects, honouring the style: `Solid` -> one strip; `Dashed`/`Dotted`
/// -> a row of short segments (dotted uses square dots, dashed uses
/// ~3x-long dashes); `Double` -> two thin strips with a gap.
///
/// `horizontal` picks which of (width, height) is the run direction.
#[allow(clippy::too_many_arguments)]
fn push_edge(
    x: f32,
    y: f32,
    length: f32,
    thickness: f32,
    horizontal: bool,
    style: BorderStyle,
    color: Color,
    items: &mut Vec<DisplayItem>,
) {
    if length <= 0.0 || thickness <= 0.0 {
        return;
    }
    let strip = |off_main: f32, cross_off: f32, main_len: f32, cross_len: f32, items: &mut Vec<DisplayItem>| {
        let (rx, ry, rw, rh) = if horizontal {
            (x + off_main, y + cross_off, main_len, cross_len)
        } else {
            (x + cross_off, y + off_main, cross_len, main_len)
        };
        items.push(rect(rx, ry, rw, rh, color));
    };

    match style {
        BorderStyle::Solid | BorderStyle::None => strip(0.0, 0.0, length, thickness, items),
        BorderStyle::Double => {
            let t = (thickness / 3.0).max(1.0);
            strip(0.0, 0.0, length, t, items);
            strip(0.0, thickness - t, length, t, items);
        }
        BorderStyle::Dotted | BorderStyle::Dashed => {
            let seg = if style == BorderStyle::Dotted { thickness.max(1.0) } else { (thickness * 3.0).max(3.0) };
            let gap = thickness.max(1.0);
            let step = seg + gap;
            let mut pos = 0.0;
            while pos < length {
                let this = seg.min(length - pos);
                strip(pos, 0.0, this, thickness, items);
                pos += step;
            }
        }
    }
}

/// Four-strip frame just inside `(x, y, w, h)` with strips `thickness`
/// px wide, drawn in `style`. Used for both `border` (at the border
/// box) and `outline` (at an outset box).
#[allow(clippy::too_many_arguments)]
fn push_frame(x: f32, y: f32, w: f32, h: f32, thickness: f32, style: BorderStyle, color: Color, items: &mut Vec<DisplayItem>) {
    let t = thickness;
    // Top & bottom span the full width; left & right are inset so the
    // corners meet without overlap.
    push_edge(x, y, w, t, true, style, color, items);
    push_edge(x, y + (h - t).max(0.0), w, t, true, style, color, items);
    push_edge(x, y + t, (h - 2.0 * t).max(0.0), t, false, style, color, items);
    push_edge(x + (w - t).max(0.0), y + t, (h - 2.0 * t).max(0.0), t, false, style, color, items);
}

/// M6's gap: each side in its own width, style and colour. Top and bottom
/// span the full width; left and right fit between them, so the strips
/// meet at the corners without overlapping — for a uniform border, exactly
/// [`push_frame`]'s four strips.
fn push_border_rects(node: &LayoutBox, border: &Border, items: &mut Vec<DisplayItem>) {
    let (x, y, w, h) = (node.x, node.y, node.width, node.height);
    // A fully transparent side paints nothing: no strips for it (a dotted
    // transparent side would be hundreds of invisible rects).
    let seen = |s: style::BorderSide| if s.color.a == 0 { style::BorderSide { style: BorderStyle::None, ..s } } else { s };
    let (top, right, bottom, left) = (seen(border.top), seen(border.right), seen(border.bottom), seen(border.left));
    let push_edge = |x, y, len, side: style::BorderSide, horizontal, items: &mut Vec<DisplayItem>| {
        if side.style != BorderStyle::None {
            push_edge(x, y, len, side.width, horizontal, side.style, side.color, items);
        }
    };
    push_edge(x, y, w, top, true, items);
    push_edge(x, y + (h - bottom.width).max(0.0), w, bottom, true, items);
    let side_height = (h - top.width - bottom.width).max(0.0);
    push_edge(x, y + top.width, side_height, left, false, items);
    push_edge(x + (w - right.width).max(0.0), y + top.width, side_height, right, false, items);
}

fn push_outline_rects(node: &LayoutBox, outline: &layout::Outline, items: &mut Vec<DisplayItem>) {
    let o = outline.offset;
    let x = node.x - o - outline.width;
    let y = node.y - o - outline.width;
    let w = node.width + 2.0 * (o + outline.width);
    let h = node.height + 2.0 * (o + outline.width);
    push_frame(x, y, w, h, outline.width, outline.style, outline.color, items);
}

/// Flattens a laid-out box tree into a paint-order display list
/// (parents painted before children — correct for opaque solid-color
/// boxes; will need real stacking-context ordering once opacity/
/// transforms/z-index exist).
///
/// Takes `font`/`atlas` because text runs are rasterized here, at
/// paint time — `layout` only measured them. `atlas` is mutated
/// (new glyphs get packed in on first use); already-seen glyphs are a
/// cache hit and don't touch the atlas buffer, per `text`'s own
/// generation-tracking tests.
///
/// Returns image resources alongside the display list (Milestone 7)
/// — deduplicated by identity (`Arc::as_ptr`), so an image used in
/// ten places still appears exactly once here. The caller builds
/// `Scene.images` from this; `gpu_backend` only uploads a texture for
/// ids it hasn't seen before.
pub fn build_display_list(root: &LayoutBox, font: &Font, atlas: &mut GlyphAtlas) -> (Vec<DisplayItem>, Vec<ImageResource>) {
    build_display_list_scrolled(root, font, atlas, 0.0)
}

/// Milestone 15: same as `build_display_list`, but shifts every
/// resulting item's `y` by `-scroll_offset_y` — scrolling DOWN moves
/// content visually UP, same convention every scrollable UI uses.
///
/// Deliberately simplified to WHOLE-DOCUMENT scroll only, not real
/// per-element `overflow: auto`/`scroll` regions: there's exactly one
/// scroll offset, applied uniformly to the entire rendered output,
/// because `overflow` isn't a supported CSS property yet (nothing in
/// `style`/`layout` tracks which container is "scrollable" — see
/// milestones/milestones-01-17.md's scope notes).
/// This covers the common, by-far-most-frequent real case (the whole
/// page scrolls) — nested independently-scrolling regions (a sidebar
/// with its own scrollbar, a modal's internal scroll) are real,
/// separate, deferred work.
///
/// Applied as a final pass over already-built items. Milestone 33: the
/// shift is uniform except for `position: fixed` subtrees (which do not
/// scroll — their items are pre-shifted by `+scroll` so the final pass
/// cancels out) and `position: sticky` ones (pre-shifted by their sticky
/// offset at this scroll position).
pub fn build_display_list_scrolled(
    root: &LayoutBox,
    font: &Font,
    atlas: &mut GlyphAtlas,
    scroll_offset_y: f32,
) -> (Vec<DisplayItem>, Vec<ImageResource>) {
    build_display_list_with_scrolls(root, font, atlas, scroll_offset_y, &layout::ScrollOffsets::new())
}

/// [`build_display_list_scrolled`], with scroll containers (`overflow:
/// auto|scroll`) scrolled by `container_scrolls`. Milestone 33: a box that
/// clips (`overflow` other than `visible`) clips its descendants' items to
/// its padding box.
pub fn build_display_list_with_scrolls(
    root: &LayoutBox,
    font: &Font,
    atlas: &mut GlyphAtlas,
    scroll_offset_y: f32,
    container_scrolls: &layout::ScrollOffsets,
) -> (Vec<DisplayItem>, Vec<ImageResource>) {
    // A frame that overflowed the capped atlas skipped glyphs; start this
    // one from a clear atlas. Safe here, before any glyph run of this
    // frame has taken atlas coordinates.
    atlas.begin_frame();
    let built = build(root, font, atlas, scroll_offset_y, container_scrolls);
    let mut items = built.items;
    shift_items(&mut items, -scroll_offset_y);
    (items, built.images)
}

/// [`build_display_list_with_scrolls`], also saying whether a `position:
/// sticky` box was drawn (a frame drawn from a [`crate::PageDisplayList`]
/// cannot place one).
pub fn build_display_list_for_frame(
    root: &LayoutBox,
    font: &Font,
    atlas: &mut GlyphAtlas,
    scroll_offset_y: f32,
    container_scrolls: &layout::ScrollOffsets,
) -> (Vec<DisplayItem>, Vec<ImageResource>, bool) {
    atlas.begin_frame();
    let built = build(root, font, atlas, scroll_offset_y, container_scrolls);
    let mut items = built.items;
    shift_items(&mut items, -scroll_offset_y);
    (items, built.images, built.saw_sticky)
}

/// What one build of a layout tree produced.
pub(crate) struct Built {
    pub items: Vec<DisplayItem>,
    pub images: Vec<ImageResource>,
    /// Index ranges of `items` drawn by `position: fixed` subtrees (which do
    /// not move with the page's scroll).
    pub pinned: Vec<std::ops::Range<usize>>,
    /// Whether a `position: sticky` box was drawn (its position depends on
    /// the scroll offset).
    pub saw_sticky: bool,
    /// Milestone 39b: each element box's items, in pre-order (only when
    /// asked for — a page's list keeps them).
    pub ranges: Vec<ItemRange>,
    /// Boxes built, and boxes whose items were moved over from a previous
    /// list instead (Milestone 39b).
    pub built_boxes: usize,
    pub copied_boxes: usize,
}

/// Milestone 39b (ADR 022): where an element box's items are in a page's
/// list, and where the box was (document space) when they were built.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemRange {
    pub key: layout::NodeId,
    pub start: u32,
    pub end: u32,
    /// How many ranges after this one are its descendants'.
    pub nested: u32,
    pub x: f32,
    pub y: f32,
    /// Milestone 39c: where the box paints among its siblings — `Some(z)`
    /// when positioned (after the in-flow ones, by `z-index`), `None` in
    /// tree order. A box whose layer changes moves in the list.
    pub layer: Option<i32>,
}

impl ItemRange {
    /// `start` of a box whose items were dropped with an empty group: they
    /// cannot be moved over.
    pub const DROPPED: u32 = u32::MAX;
}

/// The previous build a page's list is updated from: its items (moved out
/// as they are taken), ranges and images.
pub(crate) struct Previous<'p> {
    pub items: &'p mut [DisplayItem],
    /// Per item: drawn by a `fixed` subtree (see [`Built::pinned`]).
    pub pinned: &'p [bool],
    pub ranges: &'p [ItemRange],
    /// Where the next range is expected: boxes reused in document order
    /// are found by walking the ranges, without an index.
    pub cursor: usize,
    /// Every range by key, built on the first box not found at the cursor.
    pub index: Option<std::collections::HashMap<layout::NodeId, u32>>,
    pub images: std::collections::HashMap<u64, &'p ImageResource>,
}

impl Previous<'_> {
    /// The range recorded for `key`, if any.
    fn find(&mut self, key: layout::NodeId) -> Option<usize> {
        if self.ranges.get(self.cursor).is_some_and(|r| r.key == key) {
            return Some(self.cursor);
        }
        let ranges = self.ranges;
        let index = self.index.get_or_insert_with(|| ranges.iter().enumerate().map(|(i, r)| (r.key, i as u32)).collect());
        index.get(&key).map(|&i| i as usize)
    }
}

/// Builds `root`'s items with the page scrolled by `scroll` — before the
/// final whole-page shift, which the caller applies.
pub(crate) fn build(root: &LayoutBox, font: &Font, atlas: &mut GlyphAtlas, scroll: f32, container_scrolls: &layout::ScrollOffsets) -> Built {
    build_from(root, font, atlas, scroll, container_scrolls, false, None)
}

/// [`build`], recording each element box's item range when `record`, and
/// moving over the items of boxes reused unchanged from `previous`'s
/// layout instead of building them.
pub(crate) fn build_from(
    root: &LayoutBox,
    font: &Font,
    atlas: &mut GlyphAtlas,
    scroll: f32,
    container_scrolls: &layout::ScrollOffsets,
    record: bool,
    previous: Option<&mut Previous<'_>>,
) -> Built {
    let mut images = Vec::new();
    let mut seen_image_ids = HashSet::new();
    let mut ctx = BuildContext {
        font,
        atlas,
        images: &mut images,
        seen_image_ids: &mut seen_image_ids,
        scroll,
        in_fixed: false,
        container_scrolls,
        pinned: Vec::new(),
        saw_sticky: false,
        origin: (0.0, 0.0),
        record,
        ranges: Vec::new(),
        previous,
        layout_frame: root.subtree.frame,
        built_boxes: 0,
        copied_boxes: 0,
    };
    let mut items = Vec::new();
    build_display_list_into(root, &mut ctx, &mut items);
    let (pinned, saw_sticky, ranges, built_boxes, copied_boxes) = (ctx.pinned, ctx.saw_sticky, ctx.ranges, ctx.built_boxes, ctx.copied_boxes);
    Built { items, images, pinned, saw_sticky, ranges, built_boxes, copied_boxes }
}

/// Moves every item down by `dy`.
fn shift_items(items: &mut [DisplayItem], dy: f32) {
    shift_items_by(items, 0.0, dy);
}

/// Moves every item by (`dx`, `dy`).
pub(crate) fn shift_items_by(items: &mut [DisplayItem], dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    for item in items {
        shift_item(item, dx, dy);
    }
}

/// Moves one item by (`dx`, `dy`). A transform layer is conjugated
/// (`S ∘ M ∘ S⁻¹`), so a shifted transformed subtree moves rigidly.
pub(crate) fn shift_item(item: &mut DisplayItem, dx: f32, dy: f32) {
    match item {
        DisplayItem::Rect { x, y, .. } | DisplayItem::Image { x, y, .. } | DisplayItem::Shadow { x, y, .. } => {
            *x += dx;
            *y += dy;
        }
        DisplayItem::Glyphs { instances, .. } => {
            for g in instances.iter_mut() {
                g.x += dx;
                g.y += dy;
            }
        }
        DisplayItem::PushClip { rect, .. } => {
            rect[0] += dx;
            rect[1] += dy;
        }
        DisplayItem::PushTransform(layer) => {
            layer.matrix = layer.matrix.conjugate_by_shift(dx, dy);
            // Empty bounds (nothing drawn) stay the canonical zero, and a
            // scroll layer has no pivot (Milestone 39b: a moved subtree's
            // items equal those built where it now is).
            if layer.bounds[2] > 0.0 || layer.bounds[3] > 0.0 {
                layer.bounds[0] += dx;
                layer.bounds[1] += dy;
            }
            match &mut layer.scroll {
                Some(clip) => {
                    clip[0] += dx;
                    clip[1] += dy;
                }
                None => {
                    layer.origin[0] += dx;
                    layer.origin[1] += dy;
                }
            }
        }
        DisplayItem::PushEffect { bounds, .. } => {
            bounds[0] += dx;
            bounds[1] += dy;
        }
        DisplayItem::PopClip | DisplayItem::PopTransform | DisplayItem::PopEffect => {}
    }
}

struct BuildContext<'a, 'p> {
    font: &'a Font,
    atlas: &'a mut GlyphAtlas,
    images: &'a mut Vec<ImageResource>,
    seen_image_ids: &'a mut HashSet<u64>,
    /// The page's scroll offset (for `fixed` and `sticky` boxes).
    scroll: f32,
    /// Inside a `fixed` subtree (already scroll-compensated).
    in_fixed: bool,
    /// Per-scroll-container offsets.
    container_scrolls: &'a layout::ScrollOffsets,
    /// See [`Built::pinned`].
    pinned: Vec<std::ops::Range<usize>>,
    /// See [`Built::saw_sticky`].
    saw_sticky: bool,
    /// Milestone 39b (ADR 022): the document origin of the frame the boxes
    /// being built are in. Items are built in that frame and shifted into
    /// the document by each box that offsets its children's frame.
    origin: (f32, f32),
    /// Record each element box's item range.
    record: bool,
    ranges: Vec<ItemRange>,
    /// The list this build updates, if any, and the layout frame of the
    /// tree being built (its root's): boxes from earlier frames are
    /// unchanged since the previous list (Milestone 39c).
    previous: Option<&'a mut Previous<'p>>,
    layout_frame: u32,
    built_boxes: usize,
    copied_boxes: usize,
}

/// Milestone 31: the display items of `outline` drawn around `node`'s
/// border box, shifted by `-scroll_offset_y` — how the engine paints the
/// focus ring at display-list time, so the laid-out tree it reuses across
/// frames is never modified.
pub fn outline_items(node: &LayoutBox, outline: &layout::Outline, scroll_offset_y: f32) -> Vec<DisplayItem> {
    let mut items = Vec::new();
    push_outline_rects(node, outline, &mut items);
    for item in &mut items {
        if let DisplayItem::Rect { y, .. } = item {
            *y -= scroll_offset_y;
        }
    }
    items
}

/// The axis-aligned bounds of an item: `(x, y, width, height)`.
pub fn item_bounds(item: &DisplayItem) -> (f32, f32, f32, f32) {
    let rect = |r: &[f32; 4]| (r[0], r[1], r[2], r[3]);
    match item {
        DisplayItem::Rect { x, y, width, height, .. } | DisplayItem::Image { x, y, width, height, .. } => (*x, *y, *width, *height),
        DisplayItem::Shadow { x, y, width, height, blur, .. } => (*x - blur, *y - blur, *width + 2.0 * blur, *height + 2.0 * blur),
        DisplayItem::Glyphs { instances, .. } => {
            let mut min = (f32::INFINITY, f32::INFINITY);
            let mut max = (f32::NEG_INFINITY, f32::NEG_INFINITY);
            for g in instances {
                min = (min.0.min(g.x), min.1.min(g.y));
                max = (max.0.max(g.x + g.width), max.1.max(g.y + g.height));
            }
            if instances.is_empty() {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (min.0, min.1, max.0 - min.0, max.1 - min.1)
            }
        }
        DisplayItem::PushClip { rect: r, .. } => rect(r),
        DisplayItem::PushTransform(layer) => rect(&layer.bounds),
        DisplayItem::PushEffect { bounds, .. } => rect(bounds),
        DisplayItem::PopClip | DisplayItem::PopTransform | DisplayItem::PopEffect => (0.0, 0.0, 0.0, 0.0),
    }
}

/// Milestone 34: the bounds each item draws within, in the list's own
/// (outermost) space — what culling, damage and the page list's bands go
/// by. Everything inside a transform layer takes the layer's bounds (its
/// own coordinates are before the transform), so a transformed subtree is
/// culled, banded and damaged as one unit, its push and pop included.
/// Clip and effect markers are `None`: kept always (they are few, and must
/// stay balanced).
pub fn effective_bounds(items: &[DisplayItem]) -> Vec<Option<(f32, f32, f32, f32)>> {
    let mut out = Vec::with_capacity(items.len());
    let mut depth = 0usize;
    let mut outer: (f32, f32, f32, f32) = (0.0, 0.0, 0.0, 0.0);
    for item in items {
        match item {
            DisplayItem::PushTransform(layer) => {
                if depth == 0 {
                    outer = (layer.bounds[0], layer.bounds[1], layer.bounds[2], layer.bounds[3]);
                }
                depth += 1;
                out.push(Some(outer));
            }
            DisplayItem::PopTransform => {
                out.push(Some(outer));
                depth = depth.saturating_sub(1);
            }
            _ if depth > 0 => out.push(Some(outer)),
            DisplayItem::PushClip { .. } | DisplayItem::PopClip | DisplayItem::PushEffect { .. } | DisplayItem::PopEffect => out.push(None),
            _ => out.push(Some(item_bounds(item))),
        }
    }
    out
}

/// The union of what `items` draw (in their outermost space), if anything:
/// a transform layer counts as its bounds, and nothing inside it again.
fn content_bounds(items: &[DisplayItem]) -> Option<(f32, f32, f32, f32)> {
    let mut acc: Option<(f32, f32, f32, f32)> = None;
    let mut depth = 0usize;
    for item in items {
        let b = match item {
            DisplayItem::PushTransform(layer) => {
                depth += 1;
                if depth > 1 {
                    continue;
                }
                (layer.bounds[0], layer.bounds[1], layer.bounds[2], layer.bounds[3])
            }
            DisplayItem::PopTransform => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ if depth > 0 => continue,
            DisplayItem::PushClip { .. } | DisplayItem::PopClip | DisplayItem::PushEffect { .. } | DisplayItem::PopEffect => continue,
            _ => item_bounds(item),
        };
        if !(b.0.is_finite() && b.1.is_finite() && b.2.is_finite() && b.3.is_finite()) || b.2 <= 0.0 || b.3 <= 0.0 {
            continue;
        }
        acc = Some(match acc {
            None => b,
            Some((x, y, w, h)) => {
                let (x0, y0) = (x.min(b.0), y.min(b.1));
                (x0, y0, (x + w).max(b.0 + b.2) - x0, (y + h).max(b.1 + b.3) - y0)
            }
        });
    }
    acc
}

fn union_bounds(acc: Option<(f32, f32, f32, f32)>, b: (f32, f32, f32, f32)) -> Option<(f32, f32, f32, f32)> {
    if !(b.0.is_finite() && b.1.is_finite() && b.2.is_finite() && b.3.is_finite()) || b.2 <= 0.0 || b.3 <= 0.0 {
        return acc;
    }
    Some(match acc {
        None => b,
        Some((x, y, w, h)) => {
            let (x0, y0) = (x.min(b.0), y.min(b.1));
            (x0, y0, (x + w).max(b.0 + b.2) - x0, (y + h).max(b.1 + b.3) - y0)
        }
    })
}

/// Milestone 34, the compositor's update: gives the transform layer of each
/// element in `transforms` its new transform (pivoted where the layer was
/// built), then brings every layer's bounds up to date. One pass, no layout
/// and no display list build. Returns how many layers were updated.
pub fn update_transform_layers(items: &mut [DisplayItem], transforms: &std::collections::HashMap<u64, Transform>) -> usize {
    let mut updated = 0;
    for item in items.iter_mut() {
        if let DisplayItem::PushTransform(layer) = item {
            if let (None, Some(t)) = (layer.scroll, layer.element.and_then(|e| transforms.get(&e))) {
                let matrix = crate::affine::css_transform(t, layer.origin[0], layer.origin[1]);
                layer.matrix = if matrix.is_finite() { matrix } else { crate::Affine::IDENTITY };
                updated += 1;
            }
        }
    }
    if updated > 0 {
        refresh_layer_bounds(items);
    }
    updated
}

/// Milestone 34: scrolls each scroll container's layer to its offset in
/// `scrolls` (0 when it has none) — a compositor update, like
/// [`update_transform_layers`]. Returns how many layers moved.
pub fn update_scroll_layers(items: &mut [DisplayItem], scrolls: &layout::ScrollOffsets) -> usize {
    let mut moved = 0;
    for item in items.iter_mut() {
        if let DisplayItem::PushTransform(layer) = item {
            if layer.scroll.is_some() {
                let (sx, sy) = layer.element.and_then(|e| scrolls.get(&e).copied()).unwrap_or((0.0, 0.0));
                let matrix = crate::Affine::translate(-sx, -sy);
                if matrix.is_finite() && matrix != layer.matrix {
                    layer.matrix = matrix;
                    moved += 1;
                }
            }
        }
    }
    if moved > 0 {
        refresh_layer_bounds(items);
    }
    moved
}

/// Recomputes every layer's bounds, bottom-up: a layer's content, in its own
/// space, is what it draws outside any inner layer plus each inner layer's
/// bounds — so an enclosing layer covers its inner layers where they now are.
fn refresh_layer_bounds(items: &mut [DisplayItem]) {
    // Per open layer: its push's index, and its content's bounds so far.
    type Open = (usize, Option<(f32, f32, f32, f32)>);
    let mut open: Vec<Open> = Vec::new();
    for i in 0..items.len() {
        match &mut items[i] {
            DisplayItem::PushTransform(_) => open.push((i, None)),
            DisplayItem::PopTransform => {
                let Some((push, content)) = open.pop() else { continue };
                let DisplayItem::PushTransform(layer) = &mut items[push] else { continue };
                layer.bounds = layer.bounds_of(content);
                let b = layer.bounds;
                if let Some(parent) = open.last_mut() {
                    parent.1 = union_bounds(parent.1, (b[0], b[1], b[2], b[3]));
                }
            }
            DisplayItem::PushClip { .. } | DisplayItem::PopClip | DisplayItem::PushEffect { .. } | DisplayItem::PopEffect => {}
            item => {
                if let Some(top) = open.last_mut() {
                    top.1 = union_bounds(top.1, item_bounds(item));
                }
            }
        }
    }
}

/// Milestone 31: drops every item entirely outside the viewport
/// `(0, 0)–(width, height)` (items are already in viewport space, i.e.
/// scrolled). A long page draws only what is on screen. Returns how many
/// were dropped.
pub fn cull_to_viewport(items: &mut Vec<DisplayItem>, width: f32, height: f32) -> usize {
    let before = items.len();
    let bounds = effective_bounds(items);
    let mut keep = bounds.iter().map(|b| b.is_none_or(|(x, y, w, h)| x < width && y < height && x + w > 0.0 && y + h > 0.0));
    items.retain(|_| keep.next().unwrap_or(true));
    before - items.len()
}

/// Which part of the viewport differs from the previous frame
/// (Milestone 31; consumed by damage-based presentation, Milestone 34).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Damage {
    /// Nothing to compare against (first frame, resize): repaint it all.
    Full,
    /// Identical to the previous frame.
    None,
    /// Only this rectangle changed: `x, y, width, height`.
    Rect { x: f32, y: f32, width: f32, height: f32 },
}

/// How far ahead [`damage_between`] looks to re-synchronise two display
/// lists after a difference (an inserted or removed run of items).
const DAMAGE_LOOKAHEAD: usize = 16;

/// Whether two items draw the same pixels. Like `==`, except that a glyph
/// run's `atlas_generation` is ignored: it bumps whenever ANY new glyph is
/// rasterised, but existing glyphs never move in the atlas (it only grows),
/// so an unchanged run is still unchanged. (Comparing it made typing one
/// character damage every line of text on screen.)
fn same_pixels(a: &DisplayItem, b: &DisplayItem) -> bool {
    match (a, b) {
        (DisplayItem::Glyphs { instances: ia, .. }, DisplayItem::Glyphs { instances: ib, .. }) => ia == ib,
        _ => a == b,
    }
}

/// The damage between two frames' display lists: the union of the bounds of
/// every item not matched, in order, in both. A two-pointer walk that, on a
/// mismatch, looks up to [`DAMAGE_LOOKAHEAD`] items ahead in either list to
/// re-synchronise — so separate small changes (typed text near the top, the
/// caret and focus ring appended at the end) stay separate small damage,
/// rather than everything between them counting as changed. O(n × lookahead).
pub fn damage_between(old: &[DisplayItem], new: &[DisplayItem]) -> Damage {
    let (old_bounds, new_bounds) = (effective_bounds(old), effective_bounds(new));
    // A clip or effect marker that changed changes everything inside it:
    // without its extent at hand, repaint it all.
    let mut everything = false;
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    let mut add = |b: Option<(f32, f32, f32, f32)>| {
        let Some((x, y, w, h)) = b else {
            everything = true;
            return;
        };
        bounds = Some(match bounds {
            None => (x, y, x + w, y + h),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)),
        });
    };
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if same_pixels(&old[i], &new[j]) {
            i += 1;
            j += 1;
            continue;
        }
        let ahead_in_old = old[i + 1..old.len().min(i + 1 + DAMAGE_LOOKAHEAD)].iter().position(|x| same_pixels(x, &new[j]));
        let ahead_in_new = new[j + 1..new.len().min(j + 1 + DAMAGE_LOOKAHEAD)].iter().position(|x| same_pixels(x, &old[i]));
        match (ahead_in_old, ahead_in_new) {
            // Items were removed from the old list before new[j].
            (Some(k), other) if other.is_none_or(|m| k <= m) => {
                old_bounds[i..=i + k].iter().for_each(|b| add(*b));
                i += k + 1;
            }
            // Items were inserted in the new list before old[i].
            (_, Some(m)) => {
                new_bounds[j..=j + m].iter().for_each(|b| add(*b));
                j += m + 1;
            }
            // Changed in place.
            _ => {
                add(old_bounds[i]);
                add(new_bounds[j]);
                i += 1;
                j += 1;
            }
        }
    }
    old_bounds[i..].iter().chain(&new_bounds[j..]).for_each(|b| add(*b));
    if everything {
        return Damage::Full;
    }
    match bounds {
        None => Damage::None,
        Some((x0, y0, x1, y1)) => Damage::Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 },
    }
}

/// One entry per DISTINCT image (by identity, not by content) that
/// appears anywhere in a `Scene` — see `build_display_list`'s doc
/// comment for why this is deduplicated rather than one entry per
/// `DisplayItem::Image`.
///
/// `rgba` is an `Arc::clone` of `images::DecodedImage.rgba` (same fix
/// as `GlyphAtlasSnapshot` — see scene.rs) — O(1) per frame, not a
/// fresh deep copy of a potentially multi-MB decoded image every
/// single frame regardless of whether it changed. `images::ImageCache`
/// already returns the SAME `Arc<DecodedImage>` for a given path
/// across frames, so this costs nothing extra to share.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageResource {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

fn image_id(image: &Arc<images::DecodedImage>) -> u64 {
    Arc::as_ptr(image) as *const () as u64
}

/// Where and how to draw `node`'s decoded image (natural size `nat_w` x
/// `nat_h`): the rect on screen and the part of the image that fills it.
/// `None`: nothing visible.
///
/// A content image (`<img>`) follows `object-fit` (centred), clipped to
/// its box. A background follows `background-size`, `background-position`
/// (an offset plus a percentage of the free space) and `background-repeat`:
/// on a repeating axis it covers the whole box, tiled through the UVs;
/// otherwise it is one tile, clipped to the box (Milestone 33 — overflowing
/// `cover` images and tiles used to be drawn outside their box).
fn image_placement(node: &LayoutBox, nat_w: f32, nat_h: f32) -> Option<(f32, f32, f32, f32, [f32; 4])> {
    use style::{BackgroundSize, ObjectFit};
    let (bx, by, bw, bh) = (node.x, node.y, node.width, node.height);
    if nat_w <= 0.0 || nat_h <= 0.0 || bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let contain = |bw: f32, bh: f32| {
        let s = (bw / nat_w).min(bh / nat_h);
        (nat_w * s, nat_h * s)
    };
    let cover = |bw: f32, bh: f32| {
        let s = (bw / nat_w).max(bh / nat_h);
        (nat_w * s, nat_h * s)
    };

    let (tile_x, tile_y, w, h, repeat_x, repeat_y) = if node.extras().image_is_content {
        // object-fit; object-position defaults to centre.
        let (w, h) = match node.extras().object_fit {
            ObjectFit::Fill => (bw, bh),
            ObjectFit::Contain => contain(bw, bh),
            ObjectFit::Cover => cover(bw, bh),
            ObjectFit::None => (nat_w, nat_h),
            ObjectFit::ScaleDown => {
                let (cw, ch) = contain(bw, bh);
                if cw < nat_w {
                    (cw, ch)
                } else {
                    (nat_w, nat_h)
                }
            }
        };
        (bx + (bw - w) * 0.5, by + (bh - h) * 0.5, w, h, false, false)
    } else {
        let (w, h) = match node.extras().background_size {
            BackgroundSize::Auto => (nat_w, nat_h),
            BackgroundSize::Contain => contain(bw, bh),
            BackgroundSize::Cover => cover(bw, bh),
            BackgroundSize::Explicit(w, h) => match (w.map(|l| l.resolve(bw)), h.map(|l| l.resolve(bh))) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => (w, nat_h * (w / nat_w)),
                (None, Some(h)) => (nat_w * (h / nat_h), h),
                (None, None) => (nat_w, nat_h),
            },
        };
        let p = node.extras().background_position;
        let x = bx + p.x.px + p.x.percent / 100.0 * (bw - w);
        let y = by + p.y.px + p.y.percent / 100.0 * (bh - h);
        (x, y, w, h, node.extras().background_repeat.repeats_x(), node.extras().background_repeat.repeats_y())
    };
    // A degenerate tile would make the UV range enormous; nothing sensible
    // is drawn at under a hundredth of a pixel anyway.
    if !(w.is_finite() && h.is_finite() && tile_x.is_finite() && tile_y.is_finite()) || w < 0.01 || h < 0.01 {
        return None;
    }
    let (x0, x1) = if repeat_x { (bx, bx + bw) } else { (tile_x.max(bx), (tile_x + w).min(bx + bw)) };
    let (y0, y1) = if repeat_y { (by, by + bh) } else { (tile_y.max(by), (tile_y + h).min(by + bh)) };
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let uv = [(x0 - tile_x) / w, (y0 - tile_y) / h, (x1 - tile_x) / w, (y1 - tile_y) / h];
    Some((x0, y0, x1 - x0, y1 - y0, uv))
}

fn build_display_list_into(node: &LayoutBox, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) {
    // Milestone 39b: an element box's items are recorded — or, if it was
    // moved over unchanged from the previous layout, moved over from the
    // previous list.
    if let Some(key) = node.element_key.filter(|_| ctx.record && node.text.is_none()) {
        if node.subtree.frame < ctx.layout_frame && copy_previous(node, key, ctx, items) {
            return;
        }
        let entry = ctx.ranges.len();
        let layer = node.is_positioned().then(|| node.extras().positioning.z());
        ctx.ranges.push(ItemRange { key, start: items.len() as u32, end: 0, nested: 0, x: ctx.origin.0 + node.x, y: ctx.origin.1 + node.y, layer });
        build_box_with_effects(node, ctx, items);
        let (end, nested) = (items.len() as u32, (ctx.ranges.len() - entry - 1) as u32);
        let range = &mut ctx.ranges[entry];
        (range.end, range.nested) = (end, nested);
        if node.opacity < 1.0 && ctx.ranges[entry + 1..].iter().any(|r| r.end > end) {
            // A group that drew nothing was dropped, with whatever markers
            // its descendants wrote: their items are not in the list.
            for range in &mut ctx.ranges[entry + 1..] {
                range.start = ItemRange::DROPPED;
            }
        }
        return;
    }
    build_box_with_effects(node, ctx, items);
}

/// Milestone 39b: moves `node`'s items (and its descendants' ranges) over
/// from the previous list, shifted by how far the box moved. `false` when
/// the previous list does not have them.
fn copy_previous(node: &LayoutBox, key: layout::NodeId, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) -> bool {
    let Some(previous) = ctx.previous.as_deref_mut() else { return false };
    let Some(at) = previous.find(key) else { return false };
    let old = previous.ranges[at];
    if old.start == ItemRange::DROPPED {
        return false;
    }
    previous.cursor = at + 1 + old.nested as usize;
    let at = at as u32;
    // The items are in document space; this box's are wanted in the frame
    // it is in (its ancestors shift them into the document).
    let (dx, dy) = (node.x - old.x, node.y - old.y);
    let base = items.len() as u32;
    // A moved-over `fixed` subtree (or the whole page) stays pinned.
    let mut run: Option<usize> = None;
    for (i, &pinned) in previous.pinned[old.start as usize..old.end as usize].iter().enumerate() {
        match (pinned, run) {
            (true, None) => run = Some(i),
            (false, Some(start)) => {
                ctx.pinned.push(base as usize + start..base as usize + i);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        ctx.pinned.push(base as usize + start..base as usize + (old.end - old.start) as usize);
    }
    let moved = dx != 0.0 || dy != 0.0;
    for item in &mut previous.items[old.start as usize..old.end as usize] {
        let mut item = std::mem::replace(item, DisplayItem::PopClip);
        if moved {
            shift_item(&mut item, dx, dy);
        }
        if let DisplayItem::Image { image_id, .. } = &item {
            if ctx.seen_image_ids.insert(*image_id) {
                if let Some(resource) = previous.images.get(image_id) {
                    ctx.images.push((*resource).clone());
                }
            }
        }
        items.push(item);
    }
    let (doc_dx, doc_dy) = (ctx.origin.0 + dx, ctx.origin.1 + dy);
    let first = ctx.ranges.len();
    for range in &previous.ranges[at as usize..=at as usize + old.nested as usize] {
        let start = if range.start == ItemRange::DROPPED { ItemRange::DROPPED } else { range.start - old.start + base };
        ctx.ranges.push(ItemRange { start, end: range.end - old.start + base, x: range.x + doc_dx, y: range.y + doc_dy, ..*range });
    }
    debug_assert_eq!(ctx.ranges[first].key, key);
    ctx.copied_boxes += node.subtree.boxes as usize;
    true
}

fn build_box_with_effects(node: &LayoutBox, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) {
    ctx.built_boxes += 1;
    // Milestone 34: `opacity` below 1 makes the subtree one group, drawn as
    // a layer at that opacity — overlapping descendants no longer show
    // through each other (Milestone 33 faded each item on its own).
    if node.opacity >= 1.0 {
        build_box_into(node, ctx, items);
        return;
    }
    let start = items.len();
    items.push(DisplayItem::PushEffect { opacity: node.opacity.max(0.0), bounds: [0.0; 4] });
    build_box_into(node, ctx, items);
    let bounds = content_bounds(&items[start + 1..]);
    match bounds {
        // Nothing drawn: no group.
        None => items.truncate(start),
        Some((x, y, w, h)) => {
            if let DisplayItem::PushEffect { bounds, .. } = &mut items[start] {
                *bounds = [x, y, w, h];
            }
            items.push(DisplayItem::PopEffect);
        }
    }
}

/// A clip rectangle's edges as finite numbers: an axis `overflow` does not
/// clip is unbounded (`±∞` from layout), which stays a clip that never cuts.
fn finite_clip((x0, y0, x1, y1): (f32, f32, f32, f32)) -> [f32; 4] {
    const FAR: f32 = 1.0e9;
    let (x0, y0, x1, y1) = (x0.max(-FAR), y0.max(-FAR), x1.min(FAR), y1.min(FAR));
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// Builds `node`'s items straight into `items` — its own, then its
/// children's — and applies its transform, visibility, scrolling and
/// clipping to the ranges it wrote. (Each box used to collect into vectors
/// of its own and copy them up: one allocation per box, and one more copy
/// of a subtree's items per enclosing clip, opacity or fixed box.)
#[inline]
fn build_box_into(node: &LayoutBox, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) {
    let font = ctx.font;
    // Milestone 34: a transform applies to the box and everything in it,
    // pivoted on the box's centre (`transform-origin: center`) — one
    // `PushTransform` around its own items and its children's. (Milestone
    // 14 moved only the box's own items, and could not rotate or skew.) A
    // box whose transform may animate gets its layer even at the identity,
    // so a frame can move it without a new display list.
    let transform_start = items.len();
    let transformed = node.extras.as_deref().is_some_and(|e| e.promoted || e.transform != Transform::default());
    if transformed {
        let (cx, cy) = (node.x + node.width / 2.0, node.y + node.height / 2.0);
        let matrix = crate::affine::css_transform(&node.extras().transform, cx, cy);
        let matrix = if matrix.is_finite() { matrix } else { crate::Affine::IDENTITY };
        items.push(DisplayItem::PushTransform(Box::new(TransformLayer { matrix, bounds: [0.0; 4], element: node.element_key, origin: [cx, cy], scroll: None })));
    }
    let own_start = items.len();

    // Milestone 33: outer box shadows, beneath the background, the first
    // one listed on top (so painted last).
    // Most boxes have no extras at all: skip the lookups for them.
    let extras = node.extras.as_deref();
    for shadow in extras.map_or(&[][..], |e| &e.box_shadow[..]).iter().rev().filter(|s| !s.inset && s.color.a > 0) {
        let (w, h) = (node.width + 2.0 * shadow.spread, node.height + 2.0 * shadow.spread);
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        items.push(DisplayItem::Shadow {
            x: node.x + shadow.offset_x - shadow.spread,
            y: node.y + shadow.offset_y - shadow.spread,
            width: w,
            height: h,
            color: shadow.color,
            radius: (node.border_radius + shadow.spread).max(0.0),
            blur: shadow.blur,
        });
    }

    if node.background_color.a > 0 {
        items.push(DisplayItem::Rect {
            x: node.x,
            y: node.y,
            width: node.width,
            height: node.height,
            color: node.background_color,
            radius: node.border_radius.max(0.0),
        });
    }

    // Milestone 6: border, painted as four solid strips forming a
    // frame at the border box's outer edge — reuses the existing
    // `Rect` variant rather than adding a new `DisplayItem::Border`
    // (the milestone doc explicitly allowed either approach). This
    // means zero `gpu_backend`/shader changes are needed for borders:
    // they ride the same instanced solid-quad pipeline Milestone 1
    // already built and Milestone 1 already confirmed works on real
    // hardware. Pushed AFTER the background so the border paints on
    // top of it (correct z-order for the common case where border and
    // background are different colors).
    if let Some(border) = &node.border {
        push_border_rects(node, border, items);
    }

    // Phase C: outline — painted just outside the border box (at
    // `outline-offset` px), never participates in layout. Emitted with
    // the node's own items so `transform` moves it too.
    if let Some(outline) = extras.and_then(|e| e.outline.as_ref()) {
        push_outline_rects(node, outline, items);
    }

    // Milestone 7: image, painted after background/border, before
    // text/children — matches real CSS paint order (background-image
    // sits behind an element's normal content).
    if let Some(image) = &node.image {
        let id = image_id(image);
        if ctx.seen_image_ids.insert(id) {
            ctx.images.push(ImageResource {
                id,
                width: image.width,
                height: image.height,
                rgba: Arc::clone(&image.rgba),
            });
        }
        // Phase E / Milestone 33: `object-fit` (for an <img>) or
        // `background-size` + `-position` + `-repeat` (for a background)
        // place, scale and tile the image inside its box, clipped to it.
        if let Some((dx, dy, dw, dh, uv)) = image_placement(node, image.width as f32, image.height as f32) {
            items.push(DisplayItem::Image { x: dx, y: dy, width: dw, height: dh, image_id: id, sampling: ImageSampling::boxed(uv, 1.0) });
        }
    }

    if let Some(text_box) = &node.text {
        // Milestone 35: underline and overline beneath the text, a
        // line-through above it (CSS Text Decoration 3 §2.1).
        let lines = text_box.decoration.lines;
        let decoration = |items: &mut Vec<DisplayItem>, which: u8| {
            if lines & which == 0 || text_box.color.a == 0 {
                return;
            }
            let m = font.line_metrics(&text_box.spec);
            let (offset, thickness) = match which {
                style::text::UNDERLINE => m.underline,
                style::text::OVERLINE => (-m.ascent, m.underline.1),
                _ => m.strikeout,
            };
            let thickness = thickness.max(1.0);
            items.push(DisplayItem::Rect {
                x: node.x,
                y: text_box.baseline_y + offset - thickness / 2.0,
                width: node.width,
                height: thickness,
                color: text_box.decoration.color.unwrap_or(text_box.color),
                radius: 0.0,
            });
        };
        decoration(items, style::text::UNDERLINE);
        decoration(items, style::text::OVERLINE);
        let instances = shape_text_run(node.x, text_box, font, ctx.atlas);
        if !instances.is_empty() {
            items.push(DisplayItem::Glyphs {
                instances,
                atlas_generation: ctx.atlas.generation(),
            });
        }
        decoration(items, style::text::LINE_THROUGH);
    }

    // `visibility: hidden`: nothing of its own (its children decide for
    // themselves).
    if !node.visible {
        items.truncate(own_start);
    }

    // Milestone 33: a clipping box (and a scroll container, whose content
    // moves by its scroll offset) scrolls and clips its descendants to its
    // padding box. Milestone 34: the clip is a `PushClip` the renderer
    // applies (rounded like the box's inner corners), not geometry cut here.
    let clip = node.clip_rect().map(|r| {
        let widest = node.border.as_deref().map_or(0.0, |b| [b.top.width, b.right.width, b.bottom.width, b.left.width].into_iter().fold(0.0, f32::max));
        let inner_radius = (node.border_radius - widest).max(0.0);
        (finite_clip(r), inner_radius)
    });
    if let Some((rect, radius)) = clip {
        items.push(DisplayItem::PushClip { rect, radius });
    }
    // Milestone 34: a scroll container's content is on a scroll layer —
    // translated by the scroll offset — so scrolling it moves the layer
    // instead of building the display list again.
    let scroll_layer = match (node.overflow.scrollable, node.element_key, clip) {
        (true, Some(key), Some((rect, _))) => {
            let (sx, sy) = ctx.container_scrolls.get(&key).copied().unwrap_or((0.0, 0.0));
            items.push(DisplayItem::PushTransform(Box::new(TransformLayer {
                matrix: crate::Affine::translate(-sx, -sy),
                bounds: [0.0; 4],
                element: Some(key),
                origin: [0.0; 2],
                scroll: Some(rect),
            })));
            Some(items.len() - 1)
        }
        _ => None,
    };
    build_children_into(node, ctx, items);
    if let Some(start) = scroll_layer {
        let inner = content_bounds(&items[start + 1..]);
        if let DisplayItem::PushTransform(layer) = &mut items[start] {
            layer.bounds = layer.bounds_of(inner);
        }
        items.push(DisplayItem::PopTransform);
    }
    if clip.is_some() {
        items.push(DisplayItem::PopClip);
    }

    if transformed {
        let inner = content_bounds(&items[transform_start + 1..]);
        if let DisplayItem::PushTransform(layer) = &mut items[transform_start] {
            layer.bounds = layer.bounds_of(inner);
        }
        items.push(DisplayItem::PopTransform);
    }
}

/// `node`'s children's items, in paint order.
#[inline]
fn build_children_into(node: &LayoutBox, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) {
    let (ox, oy) = node.subtree.child_offset;
    if ox != 0.0 || oy != 0.0 {
        // A moved subtree (ADR 022): its children are built in their frame,
        // then shifted into the document.
        let (start, saved) = (items.len(), ctx.origin);
        ctx.origin = (saved.0 + ox, saved.1 + oy);
        build_children_in_frame(node, ctx, items);
        ctx.origin = saved;
        shift_items_by(&mut items[start..], ox, oy);
        return;
    }
    build_children_in_frame(node, ctx, items);
}

#[inline]
fn build_children_in_frame(node: &LayoutBox, ctx: &mut BuildContext, items: &mut Vec<DisplayItem>) {
    // Milestone 33: children in paint order (z-index, positioned last).
    for child in layout::paint_order(&node.children) {
        let scheme = child.position_scheme();
        let compensation = match scheme {
            style::Position::Fixed if !ctx.in_fixed => ctx.scroll,
            style::Position::Sticky => {
                ctx.saw_sticky = true;
                child.sticky_offset(ctx.origin.1, ctx.scroll)
            }
            _ => 0.0,
        };
        if compensation == 0.0 && scheme != style::Position::Fixed {
            build_display_list_into(child, ctx, items);
            continue;
        }
        let start = items.len();
        let was_fixed = ctx.in_fixed;
        ctx.in_fixed |= scheme == style::Position::Fixed;
        build_display_list_into(child, ctx, items);
        ctx.in_fixed = was_fixed;
        shift_items(&mut items[start..], compensation);
        if scheme == style::Position::Fixed && !was_fixed {
            // The outermost fixed box: none of its items scroll with the page.
            ctx.pinned.push(start..items.len());
        }
    }
}

/// Turns a word into per-glyph draw instances: shaped exactly as layout
/// measured it (Milestone 35: kerning, ligatures, complex scripts, font
/// fallback), packing glyphs into the atlas as it goes. The glyphs come in
/// visual order; `box_x` is where the word starts.
fn shape_text_run(box_x: f32, text_box: &layout::TextBox, font: &Font, atlas: &mut GlyphAtlas) -> Vec<GlyphInstance> {
    let run = font.shape(&text_box.content, &text_box.spec);
    let mut instances = Vec::with_capacity(run.glyphs.len());
    for g in &run.glyphs {
        let entry = atlas.get_or_insert_glyph(font, g.glyph, text_box.font_size);
        if entry.atlas_w > 0 && entry.atlas_h > 0 {
            instances.push(GlyphInstance {
                x: box_x + g.x + entry.bearing_x as f32,
                y: text_box.baseline_y + g.y - entry.bearing_y as f32 - entry.atlas_h as f32,
                width: entry.atlas_w as f32,
                height: entry.atlas_h as f32,
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                atlas_w: entry.atlas_w,
                atlas_h: entry.atlas_h,
                color: text_box.color,
            });
        }
    }
    instances
}

/// What each rect of `items` covers on screen — through its transform
/// (bounding box) and cut to its clip — as `(red, x, y, w, h)`; rects cut
/// away entirely are left out. Tests read a marker-based list with it.
#[cfg(test)]
pub(crate) fn visible_rects(items: &[DisplayItem]) -> Vec<(u8, f32, f32, f32, f32)> {
    let mut out = Vec::new();
    crate::walk::walk(items, |step| {
        if let crate::walk::Step::Draw { item: DisplayItem::Rect { x, y, width, height, color, .. }, transform, clip } = step {
            let (x, y, w, h) = transform.map_rect((*x, *y, *width, *height));
            let (x0, y0) = (x.max(clip.rect[0]), y.max(clip.rect[1]));
            let (x1, y1) = ((x + w).min(clip.rect[2]), (y + h).min(clip.rect[3]));
            if x1 > x0 && y1 > y0 {
                out.push((color.r, x0, y0, x1 - x0, y1 - y0));
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use layout::Outline;
    use style::Color;

    fn test_font() -> Font {
        Font::dejavu_sans()
    }

    fn plain_box(w: f32, h: f32) -> LayoutBox {
        LayoutBox {
            x: 0.0,
            y: 0.0,
            width: w,
            height: h,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        }
    }

    fn rects(items: &[DisplayItem]) -> Vec<(f32, f32, f32, f32)> {
        items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Rect { x, y, width, height, .. } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .collect()
    }

    // ---- Phase C: border styles, outline, border-radius ----

    /// M6's gap: a side's strip in its own width and colour; a side with no
    /// style or a transparent colour paints nothing; the side strips fit
    /// between the top and bottom ones.
    #[test]
    fn each_border_side_paints_in_its_own_width_and_colour() {
        let mut node = plain_box(100.0, 50.0);
        let red = Color { r: 255, g: 0, b: 0, a: 255 };
        let mut border = Border::uniform(0.0, BorderStyle::None, red);
        border.top = style::BorderSide { width: 3.0, style: BorderStyle::Solid, color: red };
        border.left = style::BorderSide { width: 5.0, style: BorderStyle::Solid, color: Color { r: 0, g: 0, b: 255, a: 255 } };
        border.right = style::BorderSide { width: 2.0, style: BorderStyle::Solid, color: Color { r: 0, g: 0, b: 0, a: 0 } };
        node.border = Some(Box::new(border));
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        let strips: Vec<_> = rects(&items).into_iter().filter(|r| r.2 < 100.0 || r.3 < 50.0).collect();
        assert_eq!(strips.len(), 2, "top and left only: {strips:?}");
        assert!(strips.contains(&(0.0, 0.0, 100.0, 3.0)), "{strips:?}");
        assert!(strips.contains(&(0.0, 3.0, 5.0, 47.0)), "the left strip below the top one: {strips:?}");
    }

    #[test]
    fn solid_border_still_emits_exactly_four_strips() {
        let mut node = plain_box(100.0, 50.0);
        node.border = Some(Box::new(Border::uniform(4.0, BorderStyle::Solid, Color { r: 1, g: 2, b: 3, a: 255 })));
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(rects(&items).len(), 4);
    }

    #[test]
    fn dashed_border_emits_many_segments() {
        let mut node = plain_box(100.0, 50.0);
        node.border = Some(Box::new(Border::uniform(2.0, BorderStyle::Dashed, Color { r: 1, g: 2, b: 3, a: 255 })));
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        // Far more than four rects — each edge is chopped into dashes.
        assert!(rects(&items).len() > 12, "dashed border should be many small rects, got {}", rects(&items).len());
    }

    #[test]
    fn double_border_emits_two_strips_per_edge() {
        let mut node = plain_box(100.0, 50.0);
        node.border = Some(Box::new(Border::uniform(6.0, BorderStyle::Double, Color { r: 1, g: 2, b: 3, a: 255 })));
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(rects(&items).len(), 8, "double border = 2 strips x 4 edges");
    }

    #[test]
    fn outline_frame_is_drawn_outside_the_border_box() {
        let mut node = plain_box(100.0, 50.0);
        node.x = 20.0;
        node.y = 20.0;
        node.extras_mut().outline = Some(Outline { width: 3.0, color: Color { r: 9, g: 9, b: 9, a: 255 }, style: BorderStyle::Solid, offset: 2.0 });
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        let rs = rects(&items);
        assert_eq!(rs.len(), 4);
        // The top strip starts above and left of the border box (offset + width outside).
        let min_x = rs.iter().map(|r| r.0).fold(f32::MAX, f32::min);
        let min_y = rs.iter().map(|r| r.1).fold(f32::MAX, f32::min);
        assert!(min_x < 20.0 && min_y < 20.0, "outline should extend outside the box origin");
        assert!((min_x - (20.0 - 2.0 - 3.0)).abs() < 0.01);
    }

    // ---- Phase E: object-fit / background-size / background-position ----

    fn make_decoded(w: u32, h: u32) -> std::sync::Arc<images::DecodedImage> {
        std::sync::Arc::new(images::DecodedImage { width: w, height: h, rgba: std::sync::Arc::new(vec![255; (w * h * 4) as usize]) })
    }

    fn only_image_rect(items: &[DisplayItem]) -> (f32, f32, f32, f32) {
        items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Image { x, y, width, height, .. } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .expect("expected an Image item")
    }

    #[test]
    fn object_fit_contain_letterboxes_a_wide_image_in_a_square_box() {
        let mut node = plain_box(100.0, 100.0);
        node.x = 0.0;
        node.y = 0.0;
        node.image = Some(make_decoded(200, 100)); // 2:1
        node.extras_mut().image_is_content = true;
        node.extras_mut().object_fit = style::ObjectFit::Contain;
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        let (x, y, w, h) = only_image_rect(&items);
        assert_eq!((w, h), (100.0, 50.0)); // scaled to fit width
        assert_eq!((x, y), (0.0, 25.0)); // vertically centred
    }

    #[test]
    fn object_fit_cover_fills_and_overflows() {
        let mut node = plain_box(100.0, 100.0);
        node.image = Some(make_decoded(200, 100));
        node.extras_mut().image_is_content = true;
        node.extras_mut().object_fit = style::ObjectFit::Cover;
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        // Scaled to cover (200x100), centred — and, since Milestone 33,
        // clipped to the box: the middle half of the image fills it.
        assert_eq!(only_image_rect(&items), (0.0, 0.0, 100.0, 100.0));
        assert_eq!(only_image_uv(&items), [0.25, 0.0, 0.75, 1.0]);
    }

    fn only_image_uv(items: &[DisplayItem]) -> [f32; 4] {
        match items.iter().find(|i| matches!(i, DisplayItem::Image { .. })) {
            Some(DisplayItem::Image { sampling, .. }) => ImageSampling::of(sampling).uv,
            _ => panic!("no image item"),
        }
    }

    #[test]
    fn object_fit_fill_is_the_box_rect() {
        let mut node = plain_box(120.0, 40.0);
        node.image = Some(make_decoded(10, 10));
        node.extras_mut().image_is_content = true; // default object_fit = Fill
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(only_image_rect(&items), (0.0, 0.0, 120.0, 40.0));
    }

    #[test]
    fn background_size_auto_draws_at_natural_size_top_left() {
        let mut node = plain_box(200.0, 200.0);
        node.image = Some(make_decoded(30, 20));
        node.extras_mut().image_is_content = false; // background
        node.extras_mut().background_repeat = style::BackgroundRepeat::NoRepeat;
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(only_image_rect(&items), (0.0, 0.0, 30.0, 20.0));
        assert_eq!(only_image_uv(&items), FULL_UV);
    }

    #[test]
    fn a_repeating_background_is_one_item_tiling_the_whole_box() {
        let mut node = plain_box(200.0, 50.0);
        node.image = Some(make_decoded(30, 20));
        node.extras_mut().image_is_content = false; // repeat is the initial value
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(only_image_rect(&items), (0.0, 0.0, 200.0, 50.0));
        assert_eq!(only_image_uv(&items), [0.0, 0.0, 200.0 / 30.0, 2.5]);
        // repeat-x: one row of tiles, the full width, offset by position.
        node.extras_mut().background_repeat = style::BackgroundRepeat::RepeatX;
        node.extras_mut().background_position = style::BackgroundPosition { x: style::Length::px(10.0), y: style::Length::px(5.0) };
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(only_image_rect(&items), (0.0, 5.0, 200.0, 20.0));
        assert_eq!(only_image_uv(&items), [-10.0 / 30.0, 0.0, 190.0 / 30.0, 1.0]);
        // A tile entirely outside the box draws nothing.
        node.extras_mut().background_repeat = style::BackgroundRepeat::NoRepeat;
        node.extras_mut().background_position = style::BackgroundPosition { x: style::Length::px(500.0), y: style::Length::ZERO };
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert!(!items.iter().any(|i| matches!(i, DisplayItem::Image { .. })));
        // A vanishing tile is not drawn (no unbounded UV range).
        node.extras_mut().background_position = style::BackgroundPosition::default();
        node.extras_mut().background_repeat = style::BackgroundRepeat::Repeat;
        node.extras_mut().background_size = style::BackgroundSize::Explicit(Some(style::Length::px(0.001)), None);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        assert!(!items.iter().any(|i| matches!(i, DisplayItem::Image { .. })));
    }

    #[test]
    fn background_position_center_offsets_into_the_free_space() {
        let mut node = plain_box(100.0, 100.0);
        node.image = Some(make_decoded(20, 20));
        node.extras_mut().image_is_content = false;
        node.extras_mut().background_position = style::BackgroundPosition { x: style::Length::percent(50.0), y: style::Length::percent(50.0) };
        node.extras_mut().background_repeat = style::BackgroundRepeat::NoRepeat;
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        let (x, y, w, h) = only_image_rect(&items);
        assert_eq!((w, h), (20.0, 20.0));
        assert_eq!((x, y), (40.0, 40.0)); // (100-20)*0.5
    }

    #[test]
    fn background_size_cover_scales_up_preserving_ratio() {
        let mut node = plain_box(100.0, 100.0);
        node.image = Some(make_decoded(50, 25)); // 2:1
        node.extras_mut().image_is_content = false;
        node.extras_mut().background_size = style::BackgroundSize::Cover;
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        // Scaled to 200x100 at the top-left, clipped to the box: its left
        // half shows.
        assert_eq!(only_image_rect(&items), (0.0, 0.0, 100.0, 100.0));
        assert_eq!(only_image_uv(&items), [0.0, 0.0, 0.5, 1.0]);
    }

    #[test]
    fn border_radius_flows_onto_the_background_rect_only() {
        let mut node = plain_box(80.0, 40.0);
        node.background_color = Color { r: 10, g: 20, b: 30, a: 255 };
        node.border_radius = 12.0;
        node.border = Some(Box::new(Border::uniform(2.0, BorderStyle::Solid, Color { r: 0, g: 0, b: 0, a: 255 })));
        let mut atlas = GlyphAtlas::new(64, 64);
        let (items, _) = build_display_list(&node, &test_font(), &mut atlas);
        let radii: Vec<f32> = items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Rect { radius, .. } => Some(*radius),
                _ => None,
            })
            .collect();
        // background rect carries 12.0; the 4 border strips stay square.
        assert_eq!(radii.iter().filter(|r| **r == 12.0).count(), 1);
        assert_eq!(radii.iter().filter(|r| **r == 0.0).count(), 4);
    }

    #[test]
    fn transparent_boxes_are_skipped() {
        let root = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, images) = build_display_list(&root, &test_font(), &mut atlas);
        assert_eq!(items, vec![]);
        assert_eq!(images, vec![]);
    }

    #[test]
    fn parent_then_children_order() {
        let child = LayoutBox {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
            background_color: Color { r: 0, g: 255, b: 0, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let root = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color { r: 255, g: 0, b: 0, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![child],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&root, &test_font(), &mut atlas);
        assert_eq!(items.len(), 2);
        let DisplayItem::Rect { width: w0, .. } = items[0] else { panic!() };
        let DisplayItem::Rect { width: w1, .. } = items[1] else { panic!() };
        assert_eq!(w0, 10.0);
        assert_eq!(w1, 2.0);
    }

    #[test]
    fn text_box_produces_a_glyphs_item_with_one_instance_per_visible_char() {
        let font = test_font();
        let metrics = font.measure("Hi", 16.0);
        let text_box = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: metrics.width,
            height: metrics.line_height,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            extras: None,
            element_key: None,
            text: Some(layout::TextBox {
                content: "Hi".to_string(),
                font_size: 16.0,
                color: Color { r: 0, g: 0, b: 0, a: 255 },
                baseline_y: metrics.ascent,
                spec: text::TextSpec::plain(16.0),
                decoration: style::Decoration::default(),
            }),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&text_box, &font, &mut atlas);
        assert_eq!(items.len(), 1);
        let DisplayItem::Glyphs { instances, .. } = &items[0] else {
            panic!("expected a Glyphs item")
        };
        // "Hi" — both characters have visible bitmaps, so 2 instances.
        assert_eq!(instances.len(), 2);
    }

    #[test]
    fn glyph_instances_advance_left_to_right() {
        let font = test_font();
        let metrics = font.measure("II", 24.0);
        let text_box = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: metrics.width,
            height: metrics.line_height,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            extras: None,
            element_key: None,
            text: Some(layout::TextBox {
                content: "II".to_string(),
                font_size: 24.0,
                color: Color { r: 0, g: 0, b: 0, a: 255 },
                baseline_y: metrics.ascent,
                spec: text::TextSpec::plain(24.0),
                decoration: style::Decoration::default(),
            }),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&text_box, &font, &mut atlas);
        let DisplayItem::Glyphs { instances, .. } = &items[0] else { panic!() };
        assert_eq!(instances.len(), 2);
        assert!(instances[1].x > instances[0].x, "second glyph must be to the right of the first");
    }

    #[test]
    fn repeated_glyph_in_same_run_reuses_atlas_entry() {
        let font = test_font();
        let metrics = font.measure("AA", 16.0);
        let text_box = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: metrics.width,
            height: metrics.line_height,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            extras: None,
            element_key: None,
            text: Some(layout::TextBox {
                content: "AA".to_string(),
                font_size: 16.0,
                color: Color { r: 0, g: 0, b: 0, a: 255 },
                baseline_y: metrics.ascent,
                spec: text::TextSpec::plain(16.0),
                decoration: style::Decoration::default(),
            }),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let _ = build_display_list(&text_box, &font, &mut atlas);
        let gen_after_first_run = atlas.generation();
        let _ = build_display_list(&text_box, &font, &mut atlas);
        // Same text, same font size, second run: nothing new to pack.
        assert_eq!(atlas.generation(), gen_after_first_run);
    }

    #[test]
    fn border_emits_four_rects_after_the_background() {
        let node = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
            background_color: Color { r: 10, g: 10, b: 10, a: 255 },
            border: Some(Box::new(Border::uniform(4.0, BorderStyle::Solid, Color { r: 255, g: 0, b: 0, a: 255 }))),
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        // 1 background rect + 4 border strips.
        assert_eq!(items.len(), 5);
        let DisplayItem::Rect { color: bg_color, .. } = items[0] else { panic!() };
        assert_eq!(bg_color, Color { r: 10, g: 10, b: 10, a: 255 });
        for border_item in &items[1..5] {
            let DisplayItem::Rect { color, .. } = border_item else { panic!() };
            assert_eq!(*color, Color { r: 255, g: 0, b: 0, a: 255 });
        }
    }

    #[test]
    fn border_strips_stay_within_the_border_box_bounds() {
        let node = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            background_color: Color::TRANSPARENT,
            border: Some(Box::new(Border::uniform(5.0, BorderStyle::Solid, Color { r: 0, g: 0, b: 0, a: 255 }))),
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        for item in &items {
            let DisplayItem::Rect { x, y, width, height, .. } = item else { continue };
            assert!(*x >= node.x - 0.01, "border rect x is outside the border box");
            assert!(*y >= node.y - 0.01, "border rect y is outside the border box");
            assert!(*x + *width <= node.x + node.width + 0.01, "border rect overflows right edge");
            assert!(*y + *height <= node.y + node.height + 0.01, "border rect overflows bottom edge");
        }
    }

    #[test]
    fn no_border_means_no_extra_rects() {
        let node = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        assert_eq!(items.len(), 1); // background only
    }

    fn make_decoded_image(w: u32, h: u32) -> Arc<images::DecodedImage> {
        Arc::new(images::DecodedImage { width: w, height: h, rgba: std::sync::Arc::new(vec![255u8; (w * h * 4) as usize]) })
    }

    #[test]
    fn image_box_emits_an_image_item_and_a_resource() {
        let img = make_decoded_image(8, 4);
        let node = LayoutBox {
            x: 5.0,
            y: 10.0,
            width: 8.0,
            height: 4.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(Arc::clone(&img)),
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, images) = build_display_list(&node, &test_font(), &mut atlas);

        assert_eq!(items.len(), 1);
        let DisplayItem::Image { x, y, width, height, image_id, .. } = items[0] else { panic!("expected an Image item") };
        assert_eq!((x, y, width, height), (5.0, 10.0, 8.0, 4.0));

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].id, image_id);
        assert_eq!(images[0].width, 8);
        assert_eq!(images[0].height, 4);
    }

    #[test]
    fn same_image_used_twice_is_deduplicated_in_the_resource_list() {
        let img = make_decoded_image(4, 4);
        let child = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(Arc::clone(&img)),
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let root = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(Arc::clone(&img)), // same Arc, used again
            element_key: None,
            text: None,
            extras: None,
            children: vec![child],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, images) = build_display_list(&root, &test_font(), &mut atlas);

        // Two Image display items (root + child)...
        let image_items: Vec<_> = items.iter().filter(|i| matches!(i, DisplayItem::Image { .. })).collect();
        assert_eq!(image_items.len(), 2);
        // ...but only ONE resource entry, since it's the same underlying image.
        assert_eq!(images.len(), 1);
    }

    /// Performance regression: `ImageResource.rgba` must be an
    /// `Arc::clone` of the decoded image's own pixel buffer, not a
    /// fresh deep copy — this is what makes rebuilding the display
    /// list every frame (as `app`'s render loop does) O(1) per static
    /// image instead of re-copying potentially multi-MB pixel data on
    /// every single redraw.
    #[test]
    fn image_resource_rgba_shares_the_decoded_images_buffer_not_a_copy() {
        let img = make_decoded_image(4, 4);
        let root = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(Arc::clone(&img)),
            element_key: None,
            text: None,
            extras: Some(Box::new(layout::BoxExtras { image_is_content: true, ..Default::default() })),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (_items, images) = build_display_list(&root, &test_font(), &mut atlas);

        assert_eq!(images.len(), 1);
        assert!(Arc::ptr_eq(&images[0].rgba, &img.rgba), "ImageResource.rgba must be the SAME allocation as the DecodedImage's, not a copy");
    }

    #[test]
    fn distinct_images_produce_distinct_resource_entries() {
        let img_a = make_decoded_image(4, 4);
        let img_b = make_decoded_image(6, 6);
        let child = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 6.0,
            height: 6.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(img_b),
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let root = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(img_a),
            element_key: None,
            text: None,
            extras: None,
            children: vec![child],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (_items, images) = build_display_list(&root, &test_font(), &mut atlas);
        assert_eq!(images.len(), 2);
        assert_ne!(images[0].id, images[1].id);
    }

    #[test]
    fn image_paints_after_background_and_border_before_children() {
        let img = make_decoded_image(2, 2);
        let text_child = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
            background_color: Color::TRANSPARENT,
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            extras: None,
            element_key: None,
            text: Some(layout::TextBox {
                content: "x".to_string(),
                font_size: 16.0,
                color: Color { r: 0, g: 0, b: 0, a: 255 },
                baseline_y: 0.0,
                spec: text::TextSpec::plain(16.0),
                decoration: style::Decoration::default(),
            }),
            children: vec![],
            subtree: Default::default(),
        };
        let node = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: Some(Box::new(Border::uniform(1.0, BorderStyle::Solid, Color { r: 2, g: 2, b: 2, a: 255 }))),
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: Some(img),
            element_key: None,
            text: None,
            extras: None,
            children: vec![text_child],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        // Expected order: background rect, 4 border rects, image, glyphs.
        assert!(matches!(items[0], DisplayItem::Rect { .. })); // background
        assert!(matches!(items[5], DisplayItem::Image { .. }));
        assert!(matches!(items[6], DisplayItem::Glyphs { .. }));
    }

    // ---- Milestone 14: transform ----

    #[test]
    fn identity_transform_leaves_geometry_unchanged() {
        let node = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        let DisplayItem::Rect { x, y, width, height, .. } = items[0] else { panic!() };
        assert_eq!((x, y, width, height), (10.0, 20.0, 100.0, 50.0));
    }

    #[test]
    fn translate_shifts_the_rect_by_exactly_the_offset() {
        let node = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: Some(Box::new(layout::BoxExtras { transform: Transform { translate_x: 5.0, translate_y: -3.0, scale_x: 1.0, scale_y: 1.0, ..Transform::default() }, ..Default::default() })),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        assert!(matches!(items[0], DisplayItem::PushTransform(_)), "a transformed box draws through a transform layer");
        assert_eq!(super::visible_rects(&items), vec![(1, 15.0, 17.0, 100.0, 50.0)]);
    }

    #[test]
    fn scale_grows_around_the_nodes_own_center() {
        let node = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: Some(Box::new(layout::BoxExtras { transform: Transform { translate_x: 0.0, translate_y: 0.0, scale_x: 2.0, scale_y: 2.0, ..Transform::default() }, ..Default::default() })),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        // Center was (50,50); doubled size is 200x200, still centered
        // on (50,50) => top-left moves to (-50,-50).
        assert_eq!(super::visible_rects(&items), vec![(1, -50.0, -50.0, 200.0, 200.0)]);
    }

    #[test]
    fn a_transform_carries_the_children_with_it() {
        let child = LayoutBox {
            x: 5.0,
            y: 5.0,
            width: 10.0,
            height: 10.0,
            background_color: Color { r: 2, g: 2, b: 2, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let parent = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 20.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: Some(Box::new(layout::BoxExtras { transform: Transform { translate_x: 100.0, translate_y: 100.0, scale_x: 1.0, scale_y: 1.0, ..Transform::default() }, ..Default::default() })),
            children: vec![child],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&parent, &test_font(), &mut atlas);
        // Milestone 34: the whole subtree moves (it used to be the box's
        // own items only).
        assert_eq!(super::visible_rects(&items), vec![(1, 100.0, 100.0, 20.0, 20.0), (2, 105.0, 105.0, 10.0, 10.0)]);
    }

    #[test]
    fn border_and_image_items_are_also_transformed() {
        let node = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            background_color: Color::TRANSPARENT,
            border: Some(Box::new(Border::uniform(5.0, BorderStyle::Solid, Color { r: 3, g: 3, b: 3, a: 255 }))),
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: Some(Box::new(layout::BoxExtras { transform: Transform { translate_x: 10.0, translate_y: 0.0, scale_x: 1.0, scale_y: 1.0, ..Transform::default() }, ..Default::default() })),
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list(&node, &test_font(), &mut atlas);
        // Top border strip should be shifted by the same translate_x.
        assert_eq!(super::visible_rects(&items)[0].1, 10.0);
    }

    // ---- Milestone 15: scroll offset ----

    #[test]
    fn scroll_offset_shifts_rect_items_upward() {
        let node = LayoutBox {
            x: 0.0,
            y: 100.0,
            width: 50.0,
            height: 50.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas = GlyphAtlas::new(256, 64);
        let (items, _images) = build_display_list_scrolled(&node, &test_font(), &mut atlas, 30.0);
        let DisplayItem::Rect { y, .. } = items[0] else { panic!() };
        assert_eq!(y, 70.0);
    }

    #[test]
    fn zero_scroll_offset_matches_the_unscrolled_build() {
        let node = LayoutBox {
            x: 0.0,
            y: 10.0,
            width: 50.0,
            height: 50.0,
            background_color: Color { r: 1, g: 1, b: 1, a: 255 },
            border: None,
            border_radius: 0.0,
            overflow: layout::OverflowClip::default(),
            opacity: 1.0,
            visible: true,
            image: None,
            element_key: None,
            text: None,
            extras: None,
            children: vec![],
            subtree: Default::default(),
        };
        let mut atlas1 = GlyphAtlas::new(256, 64);
        let mut atlas2 = GlyphAtlas::new(256, 64);
        let (unscrolled, _) = build_display_list(&node, &test_font(), &mut atlas1);
        let (scrolled_by_zero, _) = build_display_list_scrolled(&node, &test_font(), &mut atlas2, 0.0);
        assert_eq!(unscrolled, scrolled_by_zero);
    }
}

#[cfg(test)]
mod damage_tests {
    use super::*;

    fn rect(x: f32, y: f32) -> DisplayItem {
        DisplayItem::Rect { x, y, width: 10.0, height: 10.0, color: style::Color { r: 1, g: 2, b: 3, a: 255 }, radius: 0.0 }
    }

    #[test]
    fn damage_is_the_union_of_what_changed() {
        let a = vec![rect(0.0, 0.0), rect(100.0, 0.0), rect(200.0, 0.0)];
        assert_eq!(damage_between(&a, &a), Damage::None);
        let mut b = a.clone();
        b[1] = rect(100.0, 50.0);
        assert_eq!(damage_between(&a, &b), Damage::Rect { x: 100.0, y: 0.0, width: 10.0, height: 60.0 }, "old and new position");
        let mut c = a.clone();
        c.push(rect(300.0, 300.0));
        assert_eq!(damage_between(&a, &c), Damage::Rect { x: 300.0, y: 300.0, width: 10.0, height: 10.0 }, "an added item");
        assert_eq!(damage_between(&c, &a), Damage::Rect { x: 300.0, y: 300.0, width: 10.0, height: 10.0 }, "a removed item");
    }

    #[test]
    fn a_new_glyph_in_the_atlas_does_not_damage_unchanged_text() {
        let run = |generation| DisplayItem::Glyphs {
            instances: vec![GlyphInstance { x: 0.0, y: 0.0, width: 5.0, height: 5.0, atlas_x: 0, atlas_y: 0, atlas_w: 5, atlas_h: 5, color: style::Color { r: 0, g: 0, b: 0, a: 255 } }],
            atlas_generation: generation,
        };
        assert_eq!(damage_between(&[run(1), rect(50.0, 50.0)], &[run(2), rect(50.0, 50.0)]), Damage::None);
    }

    #[test]
    fn separate_changes_at_both_ends_stay_separate() {
        // 100 unchanged items between a change at the start and one at the
        // end: the damage is the two changes' union, not every item.
        let mut a: Vec<DisplayItem> = (0..100).map(|i| rect(0.0, 20.0 * i as f32)).collect();
        a.insert(0, rect(500.0, 0.0));
        let mut b = a.clone();
        b.insert(1, rect(500.0, 12.0)); // inserted near the start
        b.push(rect(505.0, 5.0)); // appended at the end
        assert_eq!(damage_between(&a, &b), Damage::Rect { x: 500.0, y: 5.0, width: 15.0, height: 17.0 });
    }

    #[test]
    fn culling_keeps_only_what_intersects_the_viewport() {
        let mut items = vec![rect(0.0, 0.0), rect(0.0, -20.0), rect(0.0, -5.0), rect(0.0, 695.0), rect(0.0, 700.0), rect(-10.0, 5.0), rect(800.0, 5.0)];
        let dropped = cull_to_viewport(&mut items, 800.0, 700.0);
        assert_eq!(dropped, 4, "fully above, fully below, fully left, fully right");
        assert_eq!(items.len(), 3, "partly visible items stay");
    }
}

#[cfg(test)]
mod positioned_paint_tests {
    use super::*;
    use style::Color;
    use text::Font;

    fn test_font() -> Font {
        Font::dejavu_sans()
    }

    fn plain_box(w: f32, h: f32) -> LayoutBox {
        LayoutBox { width: w, height: h, ..LayoutBox::empty() }
    }

    // ---- Milestone 33: positioned boxes when painting ----

    fn colored(x: f32, y: f32, w: f32, h: f32, c: u8, scheme: style::Position, z: Option<i32>) -> LayoutBox {
        let mut b = plain_box(w, h);
        b.x = x;
        b.y = y;
        b.background_color = Color { r: c, g: 0, b: 0, a: 255 };
        b.extras_mut().positioning = layout::Positioning { scheme, z_index: z, sticky: None };
        b
    }

    fn rect_ys(items: &[DisplayItem]) -> Vec<(u8, f32)> {
        items.iter().filter_map(|i| match i {
            DisplayItem::Rect { y, color, .. } => Some((color.r, *y)),
            _ => None,
        }).collect()
    }

    #[test]
    fn fixed_boxes_do_not_scroll_and_sticky_ones_are_offset() {
        let mut root = plain_box(800.0, 5000.0);
        root.children.push(colored(0.0, 100.0, 10.0, 10.0, 1, style::Position::Static, None));
        root.children.push(colored(0.0, 0.0, 10.0, 10.0, 2, style::Position::Fixed, None));
        let mut sticky = colored(0.0, 200.0, 10.0, 10.0, 3, style::Position::Sticky, None);
        sticky.extras_mut().positioning.sticky = Some(Box::new(layout::Sticky { top: Some(0.0), bottom: None, room_above: 200.0, room_below: 4790.0, viewport_height: 600.0 }));
        root.children.push(sticky);
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list_scrolled(&root, &test_font(), &mut atlas, 500.0);
        let ys = rect_ys(&items);
        assert!(ys.contains(&(1, -400.0)), "in flow: scrolled {ys:?}");
        assert!(ys.contains(&(2, 0.0)), "fixed: where it was {ys:?}");
        assert!(ys.contains(&(3, 0.0)), "sticky: pinned to the top {ys:?}");
    }

    #[test]
    fn children_paint_in_z_order() {
        let mut root = plain_box(800.0, 600.0);
        root.children.push(colored(0.0, 0.0, 10.0, 10.0, 1, style::Position::Absolute, Some(5)));
        root.children.push(colored(0.0, 0.0, 10.0, 10.0, 2, style::Position::Static, None));
        root.children.push(colored(0.0, 0.0, 10.0, 10.0, 3, style::Position::Relative, Some(-1)));
        root.children.push(colored(0.0, 0.0, 10.0, 10.0, 4, style::Position::Relative, None));
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&root, &test_font(), &mut atlas);
        let order: Vec<u8> = rect_ys(&items).into_iter().map(|(c, _)| c).collect();
        assert_eq!(order, vec![3, 2, 4, 1]);
    }

    // ---- overflow ----

    fn clipper(scrollable: bool) -> LayoutBox {
        let mut b = plain_box(100.0, 50.0);
        b.element_key = Some(7);
        b.overflow = layout::OverflowClip { x: true, y: true, scrollable };
        b
    }

    #[test]
    fn a_clipping_box_clips_its_descendants_but_not_itself() {
        let mut root = clipper(false);
        root.background_color = Color { r: 9, g: 0, b: 0, a: 255 };
        root.children.push(colored(80.0, 40.0, 50.0, 50.0, 1, style::Position::Static, None));
        root.children.push(colored(200.0, 0.0, 10.0, 10.0, 2, style::Position::Static, None));
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&root, &test_font(), &mut atlas);
        assert_eq!(super::visible_rects(&items), vec![(9, 0.0, 0.0, 100.0, 50.0), (1, 80.0, 40.0, 20.0, 10.0)], "the outside child is gone");
    }

    #[test]
    fn a_scroll_container_moves_its_content_under_its_clip() {
        let mut root = clipper(true);
        root.children.push(colored(0.0, 60.0, 10.0, 10.0, 1, style::Position::Static, None));
        let mut atlas = GlyphAtlas::new(32, 32);
        let mut scrolls = layout::ScrollOffsets::new();
        scrolls.insert(7, (0.0, 30.0));
        let (items, _) = build_display_list_with_scrolls(&root, &test_font(), &mut atlas, 0.0, &scrolls);
        assert_eq!(super::visible_rects(&items), vec![(1, 0.0, 30.0, 10.0, 10.0)], "scrolled into view");
        assert_eq!(root.scroll_range(), (0.0, 20.0));
    }

    #[test]
    fn opacity_fades_the_subtree_and_hidden_boxes_paint_nothing_of_their_own() {
        let mut root = colored(0.0, 0.0, 100.0, 100.0, 1, style::Position::Static, None);
        root.opacity = 0.5;
        let mut hidden = colored(0.0, 0.0, 10.0, 10.0, 2, style::Position::Static, None);
        hidden.visible = false;
        hidden.children.push(colored(0.0, 0.0, 5.0, 5.0, 3, style::Position::Static, None));
        root.children.push(hidden);
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&root, &test_font(), &mut atlas);
        let colors: Vec<(u8, u8)> = items.iter().filter_map(|i| match i {
            DisplayItem::Rect { color, .. } => Some((color.r, color.a)),
            _ => None,
        }).collect();
        assert_eq!(colors, vec![(1, 255), (3, 255)], "the hidden box's own rect is gone, its visible child stays");
        // Milestone 34: the subtree is faded as one group (overlaps do not
        // show through each other), not item by item.
        assert!(matches!(items.first(), Some(DisplayItem::PushEffect { opacity, .. }) if *opacity == 0.5));
        assert!(matches!(items.last(), Some(DisplayItem::PopEffect)));
    }

    #[test]
    fn box_shadows_paint_beneath_the_box_first_listed_on_top() {
        let mut b = colored(10.0, 10.0, 20.0, 20.0, 1, style::Position::Static, None);
        let shadow = |x: f32, r: u8, spread: f32| style::BoxShadow { offset_x: x, offset_y: 0.0, blur: 4.0, spread, color: Color { r, g: 0, b: 0, a: 255 }, inset: false };
        b.extras_mut().box_shadow = vec![shadow(2.0, 7, 1.0), shadow(5.0, 8, 0.0)].into_boxed_slice();
        let mut atlas = GlyphAtlas::new(32, 32);
        let (items, _) = build_display_list(&b, &test_font(), &mut atlas);
        match (&items[0], &items[1], &items[2]) {
            (DisplayItem::Shadow { color: c0, .. }, DisplayItem::Shadow { x, width, color: c1, blur, .. }, DisplayItem::Rect { .. }) => {
                assert_eq!((c0.r, c1.r), (8, 7), "the first-listed shadow is painted last");
                assert_eq!((*x, *width, *blur), (11.0, 22.0, 4.0), "offset and spread");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(item_bounds(&items[1]), (7.0, 5.0, 30.0, 30.0), "bounds include the blur");
    }
}
