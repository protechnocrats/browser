//! A page's display list, built once per layout and reused by every frame
//! that only scrolls or redraws an overlay (the caret, the focus ring).
//!
//! Before this, every frame — a scroll, a caret blink — built display items
//! for the whole layout tree, shaping every glyph run of the page through
//! the atlas, and only then dropped the off-screen ones. Now the items are
//! built in document space (page scroll 0) and bucketed into horizontal
//! bands; a frame takes the bands the viewport overlaps, shifts those items
//! by the scroll offset and culls them exactly: O(items near the viewport).
//!
//! Two things the bands cannot express rebuild instead:
//! - a `position: sticky` box (where it is drawn depends on the scroll
//!   offset) — see [`PageDisplayList::has_sticky`];
//! - a build that overflowed the glyph atlas (some glyphs were skipped, and
//!   the atlas is cleared at the next build) — see
//!   [`PageDisplayList::valid`].
//!
//! Milestone 34: a scroll container's scroll and a transform animation are
//! compositor updates of the list's layers, not rebuilds.
//!
//! `position: fixed` items are kept apart (they do not move with the page).
//!
//! Milestone 39b (ADR 022): a layout change updates the list instead of
//! building it again — the items of every subtree the layout moved over
//! unchanged are moved over from the previous list, shifted; only the boxes
//! laid out again are built ([`PageDisplayList::update`]).

use crate::display_list::{build_from, effective_bounds, shift_item, shift_items_by, update_scroll_layers, update_transform_layers, Built, DisplayItem, ImageResource, ItemRange, Previous};
use layout::LayoutBox;
use text::{Font, GlyphAtlas};

/// The band height, px — a few per viewport.
const BAND_HEIGHT: f32 = 256.0;
/// A page taller than this many bands gets taller bands (the index stays
/// bounded however far a page's items are spread).
const MAX_BANDS: usize = 1 << 16;
/// An item spanning more bands than this (a page-high background) is
/// checked on every frame instead of being listed in each band.
const MAX_BANDS_PER_ITEM: usize = 64;

pub struct PageDisplayList {
    /// Page-scroll-0 items, in paint order.
    items: Vec<DisplayItem>,
    /// Per item: drawn by a `position: fixed` subtree (never scrolled).
    pinned: Vec<bool>,
    images: Vec<ImageResource>,
    /// Item indices per band of document y (non-pinned items).
    bands: Vec<Vec<u32>>,
    band_origin: f32,
    band_height: f32,
    /// Pinned items, clip and effect markers, and items too tall (or not
    /// finite) to band.
    always: Vec<u32>,
    /// Each item's effective bounds (`display_list::effective_bounds`).
    bounds: Vec<Option<(f32, f32, f32, f32)>>,
    has_sticky: bool,
    atlas_overflowed: bool,
    /// The glyph atlas's placement its glyph runs were built under.
    atlas_placement: u64,
    container_scrolls: layout::ScrollOffsets,
    /// Each element box's items (Milestone 39b), in pre-order.
    ranges: Vec<ItemRange>,
    /// Boxes the last build or update built, and boxes whose items it moved
    /// over from the previous list (tests bound a change's work with them).
    pub built_boxes: usize,
    pub copied_boxes: usize,
}

impl PageDisplayList {
    /// Builds `root`'s display list with the page unscrolled and scroll
    /// containers at `container_scrolls`.
    pub fn build(root: &LayoutBox, font: &Font, atlas: &mut GlyphAtlas, container_scrolls: &layout::ScrollOffsets) -> PageDisplayList {
        // A build that overflowed the capped atlas skipped glyphs; clear it
        // before this one takes atlas coordinates (as every build does).
        atlas.begin_frame();
        let built = build_from(root, font, atlas, 0.0, container_scrolls, true, None);
        Self::from_built(built, atlas, container_scrolls)
    }

    /// Milestone 39b (ADR 022): this list, made the list of `root` — a
    /// layout of the same page that reused this list's layout where
    /// `LayoutBox::subtree.frame` says so (older than its root's). The items
    /// of those subtrees are
    /// moved over, shifted; the rest is built. Builds in full instead when
    /// that is not safe: this list is invalid or has a sticky box, or the
    /// glyph atlas was replaced, cleared or compacted since it was built
    /// (its items' glyph coordinates would be stale).
    pub fn update(mut self, root: &LayoutBox, font: &Font, atlas: &mut GlyphAtlas, container_scrolls: &layout::ScrollOffsets) -> PageDisplayList {
        if atlas.begin_frame() || !self.drawable_with(atlas) || self.has_sticky {
            let built = build_from(root, font, atlas, 0.0, container_scrolls, true, None);
            return Self::from_built(built, atlas, container_scrolls);
        }
        // Moved-over items must show the containers' current scrolls.
        self.set_container_scrolls(container_scrolls);
        let images = self.images.iter().map(|r| (r.id, r)).collect();
        let mut previous = Previous { items: &mut self.items, pinned: &self.pinned, ranges: &self.ranges, cursor: 0, index: None, images };
        let built = build_from(root, font, atlas, 0.0, container_scrolls, true, Some(&mut previous));
        Self::from_built(built, atlas, container_scrolls)
    }

    /// Milestone 39c (ADR 022 §9): after a layout patched in place at the
    /// box `path` leads to (child indices from `root`; see
    /// `layout::LayoutReuse::patched`), builds that box's items again and
    /// splices them in — every other item stays, renumbered. `false` (and
    /// the list unchanged) when that is not safe: an ancestor's markers
    /// depend on what it contains (an opacity group, a transform, a scroll
    /// layer) or it is `fixed`; the box changed paint layer (became
    /// positioned, or changed `z-index`); building the box needs a sticky box or
    /// overflows the glyph atlas; the list is not drawable. Then
    /// [`Self::update`] does it.
    pub fn patch(&mut self, root: &LayoutBox, path: &[u32], font: &Font, atlas: &mut GlyphAtlas, container_scrolls: &layout::ScrollOffsets) -> bool {
        if !self.drawable_with(atlas) || self.has_sticky || &self.container_scrolls != container_scrolls || atlas.overflowed() {
            return false;
        }
        // Down to the box, through ancestors that only position it.
        let mut node = root;
        let mut origin = (0.0f32, 0.0f32);
        for &i in path {
            let plain = node.opacity >= 1.0
                && !node.overflow.scrollable
                && !matches!(node.position_scheme(), style::Position::Fixed | style::Position::Sticky)
                && node.extras.as_deref().is_none_or(|e| !e.promoted && e.transform == style::Transform::default());
            if !plain {
                return false;
            }
            origin = (origin.0 + node.subtree.child_offset.0, origin.1 + node.subtree.child_offset.1);
            let Some(child) = node.children.get(i as usize) else { return false };
            node = child;
        }
        let Some(key) = node.element_key else { return false };
        let Some(at) = self.ranges.iter().position(|r| r.key == key) else { return false };
        let old = self.ranges[at];
        // A box that changed paint layer paints elsewhere among its siblings.
        if old.start == ItemRange::DROPPED || old.layer != node.is_positioned().then(|| node.extras().positioning.z()) {
            return false;
        }
        let built = build_from(node, font, atlas, 0.0, container_scrolls, true, None);
        if built.saw_sticky || !built.pinned.is_empty() || atlas.overflowed() {
            return false;
        }
        // Built in the frame the box is in: into the document.
        let mut items = built.items;
        shift_items_by(&mut items, origin.0, origin.1);
        let (start, old_end) = (old.start as usize, old.end as usize);
        let delta = items.len() as i64 - (old_end - start) as i64;
        let shift = |i: u32| (i64::from(i) + delta) as u32;
        // The ranges: the box's replaced, those after it renumbered, its
        // ancestors' grown or shrunk.
        let new_ranges: Vec<ItemRange> = built
            .ranges
            .into_iter()
            .map(|r| ItemRange {
                start: if r.start == ItemRange::DROPPED { r.start } else { r.start + old.start },
                end: r.end + old.start,
                x: r.x + origin.0,
                y: r.y + origin.1,
                ..r
            })
            .collect();
        let replaced = old.nested as usize + 1;
        let nested_delta = new_ranges.len() as i64 - replaced as i64;
        for (i, r) in self.ranges.iter_mut().enumerate() {
            if i < at {
                if i + r.nested as usize >= at {
                    // An ancestor's range.
                    r.end = shift(r.end);
                    r.nested = (i64::from(r.nested) + nested_delta) as u32;
                }
            } else if i >= at + replaced {
                if r.start != ItemRange::DROPPED {
                    r.start = shift(r.start);
                }
                r.end = shift(r.end);
            }
        }
        self.ranges.splice(at..at + replaced, new_ranges);
        // The items, their pinned flags and bounds.
        let new_bounds = effective_bounds(&items);
        let count = items.len();
        self.items.splice(start..old_end, items);
        self.pinned.splice(start..old_end, std::iter::repeat_n(false, count));
        self.bounds.splice(start..old_end, new_bounds);
        // The bands: the old items out, every later index renumbered, the
        // new items in.
        let (old_start, old_end) = (old.start, old.end);
        let renumber = |list: &mut Vec<u32>| {
            list.retain(|&i| i < old_start || i >= old_end);
            for i in list.iter_mut() {
                if *i >= old_end {
                    *i = shift(*i);
                }
            }
        };
        for band in &mut self.bands {
            renumber(band);
        }
        renumber(&mut self.always);
        for i in start..start + count {
            self.band_item(i);
        }
        for image in built.images {
            if !self.images.iter().any(|r| r.id == image.id) {
                self.images.push(image);
            }
        }
        self.built_boxes = built.built_boxes;
        self.copied_boxes = 0;
        true
    }

    /// Lists item `i` in the bands its bounds overlap (or as always
    /// visited), as [`Self::index`] does.
    fn band_item(&mut self, i: usize) {
        let span = self.bounds[i].and_then(|(_, y, _, h)| (!self.pinned[i] && y.is_finite() && h.is_finite()).then_some((y, y + h.max(0.0))));
        let Some((top, bottom)) = span.filter(|_| !self.bands.is_empty()) else {
            self.always.push(i as u32);
            return;
        };
        let (first, last) = (self.band_of(top), self.band_of(bottom));
        if last - first >= MAX_BANDS_PER_ITEM {
            self.always.push(i as u32);
            return;
        }
        for band in &mut self.bands[first..=last] {
            band.push(i as u32);
        }
    }

    fn from_built(built: Built, atlas: &GlyphAtlas, container_scrolls: &layout::ScrollOffsets) -> PageDisplayList {
        let mut pinned = vec![false; built.items.len()];
        for range in &built.pinned {
            pinned[range.clone()].fill(true);
        }
        let mut list = PageDisplayList {
            items: built.items,
            pinned,
            images: built.images,
            bands: Vec::new(),
            band_origin: 0.0,
            band_height: BAND_HEIGHT,
            always: Vec::new(),
            bounds: Vec::new(),
            has_sticky: built.saw_sticky,
            atlas_overflowed: atlas.overflowed(),
            atlas_placement: atlas.placement(),
            container_scrolls: container_scrolls.clone(),
            ranges: built.ranges,
            built_boxes: built.built_boxes,
            copied_boxes: built.copied_boxes,
        };
        list.index();
        list
    }

    fn index(&mut self) {
        // Milestone 34: by effective bounds — a transformed subtree bands as
        // one unit; clip and effect markers (no bounds) are always visited.
        self.bounds = effective_bounds(&self.items);
        let spans: Vec<Option<(f32, f32)>> = self
            .bounds
            .iter()
            .zip(&self.pinned)
            .map(|(b, pinned)| {
                let (_, y, _, h) = (*b)?;
                (!pinned && y.is_finite() && h.is_finite()).then_some((y, y + h.max(0.0)))
            })
            .collect();
        let (lo, hi) = spans.iter().flatten().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), (a, b)| (lo.min(*a), hi.max(*b)));
        if lo.is_finite() && hi.is_finite() {
            self.band_origin = lo;
            self.band_height = BAND_HEIGHT.max((hi - lo) / MAX_BANDS as f32);
            let count = (((hi - lo) / self.band_height) as usize + 1).min(MAX_BANDS + 1);
            self.bands = vec![Vec::new(); count];
        }
        for (i, span) in spans.into_iter().enumerate() {
            let i = i as u32;
            let Some((top, bottom)) = span else {
                self.always.push(i);
                continue;
            };
            let (first, last) = (self.band_of(top), self.band_of(bottom));
            if last - first >= MAX_BANDS_PER_ITEM {
                self.always.push(i);
                continue;
            }
            for band in &mut self.bands[first..=last] {
                band.push(i);
            }
        }
    }

    fn band_of(&self, y: f32) -> usize {
        let band = ((y - self.band_origin) / self.band_height).floor();
        if band <= 0.0 {
            0
        } else {
            (band as usize).min(self.bands.len().saturating_sub(1))
        }
    }

    /// Milestone 34: moves the transform layers of the elements in
    /// `transforms` (a compositor animation frame) and re-bands the items by
    /// their new bounds — O(items), no layout, no glyphs shaped. Returns how
    /// many layers moved.
    pub fn update_transforms(&mut self, transforms: &std::collections::HashMap<u64, style::Transform>) -> usize {
        let moved = update_transform_layers(&mut self.items, transforms);
        if moved > 0 {
            self.bands.clear();
            self.always.clear();
            self.index();
        }
        moved
    }

    /// Milestone 34: scrolls the scroll containers' layers to
    /// `container_scrolls` (a compositor update: a container scroll used to
    /// build the list again). Returns how many layers moved.
    pub fn set_container_scrolls(&mut self, container_scrolls: &layout::ScrollOffsets) -> usize {
        if &self.container_scrolls == container_scrolls {
            return 0;
        }
        let moved = update_scroll_layers(&mut self.items, container_scrolls);
        self.container_scrolls = container_scrolls.clone();
        if moved > 0 {
            self.bands.clear();
            self.always.clear();
            self.index();
        }
        moved
    }

    /// Whether frames can be drawn from this list (else it must be built
    /// again): not when its build overflowed the glyph atlas.
    pub fn valid(&self) -> bool {
        !self.atlas_overflowed
    }

    /// Milestone 39b: [`Self::valid`], and its glyph runs still point at
    /// their glyphs in `atlas` — the atlas it was built with, not cleared,
    /// compacted or replaced since.
    pub fn drawable_with(&self, atlas: &GlyphAtlas) -> bool {
        self.valid() && self.atlas_placement == atlas.placement()
    }

    /// Whether a `position: sticky` box was drawn: then no frame can be
    /// drawn from this list at a scroll offset.
    pub fn has_sticky(&self) -> bool {
        self.has_sticky
    }

    /// Every item (page scroll 0).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn images(&self) -> &[ImageResource] {
        &self.images
    }

    /// The items on screen with the page scrolled down by `scroll`, in
    /// viewport space and paint order — exactly what building at `scroll`
    /// and culling to the `width` × `height` viewport gives.
    pub fn visible(&self, scroll: f32, width: f32, height: f32) -> Vec<DisplayItem> {
        let mut candidates: Vec<u32> = self.always.clone();
        if !self.bands.is_empty() && scroll.is_finite() && height.is_finite() {
            let (first, last) = (self.band_of(scroll), self.band_of(scroll + height));
            for band in &self.bands[first..=last] {
                candidates.extend_from_slice(band);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        let mut out = Vec::with_capacity(candidates.len());
        for i in candidates {
            let i = i as usize;
            let dy = if self.pinned[i] { 0.0 } else { -scroll };
            let visible = match self.bounds[i] {
                None => true,
                Some((x, y, w, h)) => x < width && y + dy < height && x + w > 0.0 && y + dy + h > 0.0,
            };
            if visible {
                let mut item = self.items[i].clone();
                if dy != 0.0 {
                    shift_item(&mut item, 0.0, dy);
                }
                out.push(item);
            }
        }
        out
    }

    /// How many items [`Self::visible`] looked at for this viewport (tests
    /// bound the per-frame work with it).
    pub fn candidates(&self, scroll: f32, height: f32) -> usize {
        let mut n = self.always.len();
        if !self.bands.is_empty() {
            let (first, last) = (self.band_of(scroll), self.band_of(scroll + height));
            n += self.bands[first..=last].iter().map(Vec::len).sum::<usize>();
        }
        n
    }
}
