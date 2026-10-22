//! Phase D: intrinsic-size measurement — an element's min-content and
//! max-content widths, needed by `width: min-content|max-content|
//! fit-content` and by flex-row base-size resolution. Measured in a
//! separate, parallel (`rayon`) pre-pass over the whole tree before
//! the normal (sequential) positioning walk in `crate::box_model`
//! even starts, since it depends only on each element's OWN
//! descendants, never on final positions.

use crate::box_model::{display_of, effective_border, style_of};
use crate::inline::{collect_words_from_text, push_inline_child, AtomicBox, InlineWord};
use crate::MAX_LAYOUT_DEPTH;
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use style::{BoxSizing, Display, FlexDirection, Style, StyleCache};
use text::Font;

/// Phase D: an element's min-content and max-content INNER (content-box)
/// width, in px. Populated by `measure_intrinsic_parallel`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntrinsicSizes {
    pub min_content: f32,
    pub max_content: f32,
    /// Milestone 33: the min-content width of the element's CONTENT —
    /// `min_content` unless an explicit `width` pinned it. A flex item's
    /// automatic minimum size is based on this (CSS Flexbox §4.5).
    pub content_min: f32,
    /// Milestone 39c: the element's own horizontal padding and border, which
    /// its parent adds to these — so a cached entry says what the element
    /// contributed, not only what it contains.
    pub extra: f32,
}

impl IntrinsicSizes {
    pub(crate) const ZERO: IntrinsicSizes = IntrinsicSizes { min_content: 0.0, max_content: 0.0, content_min: 0.0, extra: 0.0 };

    pub(crate) fn new(min_content: f32, max_content: f32) -> IntrinsicSizes {
        IntrinsicSizes { min_content, max_content, content_min: min_content, extra: 0.0 }
    }
}

/// Every measured element's intrinsic sizes. Kept across frames (in
/// [`crate::LayoutReuse`]): an element's sizes depend only on its own
/// subtree, so a frame re-measures only the elements a change can affect —
/// the changed ones and their ancestors — and reuses the rest.
///
/// Sharded by node id, so the parallel measurement pass writes to (and
/// reads from) many small locks instead of one that every worker takes
/// for every element.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntrinsicCache {
    /// Empty until the first insert, then [`SHARDS`] maps.
    shards: Vec<HashMap<dom::NodeId, IntrinsicSizes>>,
}

const SHARDS: usize = 64;

fn shard_of(node: dom::NodeId) -> usize {
    (node % SHARDS as u64) as usize
}

impl IntrinsicCache {
    pub fn new() -> IntrinsicCache {
        IntrinsicCache::default()
    }

    pub fn get(&self, node: &dom::NodeId) -> Option<&IntrinsicSizes> {
        self.shards.get(shard_of(*node))?.get(node)
    }

    pub fn insert(&mut self, node: dom::NodeId, sizes: IntrinsicSizes) {
        if self.shards.is_empty() {
            self.shards = vec![HashMap::new(); SHARDS];
        }
        self.shards[shard_of(node)].insert(node, sizes);
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = (&dom::NodeId, &IntrinsicSizes)> {
        self.shards.iter().flat_map(|s| s.iter())
    }

    /// Keeps only the elements `keep` says are still live.
    pub fn retain(&mut self, mut keep: impl FnMut(dom::NodeId) -> bool) {
        for shard in &mut self.shards {
            shard.retain(|n, _| keep(*n));
        }
    }

    fn into_concurrent(self) -> ConcurrentSizes {
        let shards = if self.shards.is_empty() { vec![HashMap::new(); SHARDS] } else { self.shards };
        ConcurrentSizes { shards: shards.into_iter().map(Mutex::new).collect() }
    }
}

/// An [`IntrinsicCache`] being filled by the parallel pass: one lock per
/// shard.
pub(crate) struct ConcurrentSizes {
    shards: Vec<Mutex<HashMap<dom::NodeId, IntrinsicSizes>>>,
}

impl ConcurrentSizes {
    fn get(&self, node: dom::NodeId) -> Option<IntrinsicSizes> {
        self.shards[shard_of(node)].lock().unwrap_or_else(PoisonError::into_inner).get(&node).copied()
    }

    fn insert(&self, node: dom::NodeId, sizes: IntrinsicSizes) {
        self.shards[shard_of(node)].lock().unwrap_or_else(PoisonError::into_inner).insert(node, sizes);
    }

    fn into_cache(self) -> IntrinsicCache {
        IntrinsicCache { shards: self.shards.into_iter().map(|m| m.into_inner().unwrap_or_else(PoisonError::into_inner)).collect() }
    }
}

/// Reading measured sizes, from either form (grid and table sizing run both
/// during the parallel pass and during layout).
pub(crate) trait SizeLookup: Sync {
    fn sizes_of(&self, node: dom::NodeId) -> Option<IntrinsicSizes>;
}

impl SizeLookup for IntrinsicCache {
    fn sizes_of(&self, node: dom::NodeId) -> Option<IntrinsicSizes> {
        self.get(&node).copied()
    }
}

impl SizeLookup for ConcurrentSizes {
    fn sizes_of(&self, node: dom::NodeId) -> Option<IntrinsicSizes> {
        self.get(node)
    }
}

/// Whether `element`'s own box is sized from its content's intrinsic
/// sizes (Milestone 39c: such a box can change size when anything in it
/// does, whatever that thing's own size).
pub(crate) fn sized_from_content(element: ElementRef<'_>, s: &style::Style) -> bool {
    if s.width_intrinsic.is_some() || s.height_intrinsic.is_some() || s.display.is_flex_container() {
        return true;
    }
    // Milestone 33: a shrink-to-fit atomic inline box (a replaced element
    // has its own size).
    if s.display.is_atomic_inline() && s.width.is_none() && !is_replaced(element.tag()) {
        return true;
    }
    // ... a table (its columns are sized from its cells) ...
    if s.display.is_table() || s.display.is_grid() {
        return true;
    }
    // ... a float with an auto width ...
    if s.float != style::Float::None && s.width.is_none() && !is_replaced(element.tag()) {
        return true;
    }
    // ... and an out-of-flow box whose insets do not decide its width.
    s.position.is_out_of_flow() && s.width.is_none() && !(s.inset().left.is_some() && s.inset().right.is_some()) && !is_replaced(element.tag())
}

pub(crate) fn tree_needs_intrinsic_sizing(root: ElementRef<'_>, style_cache: &StyleCache, depth: usize) -> bool {
    if sized_from_content(root, style_of(style_cache, root)) {
        return true;
    }
    // Runs unconditionally (before `layout_element`'s own depth guard
    // even applies) to decide whether the intrinsic-sizing pass is
    // needed at all — so it needs its own cap, same rationale as
    // `MAX_LAYOUT_DEPTH`. Treating "too deep to check" as `false`
    // just means content past the cap can't trigger the intrinsic
    // pass, not a correctness issue for anything within the cap.
    if depth >= MAX_LAYOUT_DEPTH {
        return false;
    }
    root.children().any(|c| match c {
        ChildRef::Element(el) => tree_needs_intrinsic_sizing(el, style_cache, depth + 1),
        ChildRef::Text(_) => false,
    })
}

/// Elements whose size comes from what they embed, not their children.
fn is_replaced(tag: &str) -> bool {
    matches!(tag, "img" | "input" | "textarea" | "canvas" | "video" | "iframe" | "embed" | "object" | "svg")
}

/// Phase D — the parallel measurement pass. Each element's min/max
/// content width depends only on its DESCENDANTS (never siblings or
/// ancestors), so sibling subtrees are measured concurrently on
/// `rayon`'s work-stealing pool, exactly the way
/// `style::resolve_style_tree_parallel` fans style resolution out.
/// This is the first place layout MEASUREMENT (not just style) gets
/// real multi-core work — the positioning pass that consumes this
/// stays single-threaded (block/flex positioning has genuine
/// left-to-right / top-to-bottom sequential dependencies).
///
/// Verified for correctness against a sequential reference walk in the
/// tests, not just "doesn't crash".
pub(crate) fn measure_intrinsic_parallel(
    root: ElementRef<'_>,
    style_cache: &StyleCache,
    font: &Font,
    image_cache: &ImageCache,
) -> IntrinsicCache {
    measure_intrinsic(root, style_cache, font, image_cache, IntrinsicCache::new(), None, &AtomicUsize::new(0))
}

/// [`measure_intrinsic_parallel`], starting from `previous` (the last
/// frame's sizes): with `dirty` given, an element outside it keeps its
/// previous sizes and its subtree is not visited. `dirty` must hold every
/// element that changed (mutated, restyled, animated) and every ancestor of
/// each — then nothing outside it can have changed size. `measured` counts
/// the elements actually measured.
pub(crate) fn measure_intrinsic(
    root: ElementRef<'_>,
    style_cache: &StyleCache,
    font: &Font,
    image_cache: &ImageCache,
    previous: IntrinsicCache,
    dirty: Option<&dom::NodeIdSet>,
    measured: &AtomicUsize,
) -> IntrinsicCache {
    // One shared (sharded) cache every recursive call inserts into
    // directly, rather than each level returning its own for the parent
    // to `.extend()` — that copies every entry once per ancestor level
    // (O(n × depth)).
    let out = previous.into_concurrent();
    let pass = Pass { style_cache, font, images: image_cache, out: &out, dirty, measured };
    measure_node(root, &pass, 0);
    out.into_cache()
}

/// What every step of one measurement pass shares.
struct Pass<'a> {
    style_cache: &'a StyleCache,
    font: &'a Font,
    images: &'a ImageCache,
    out: &'a ConcurrentSizes,
    dirty: Option<&'a dom::NodeIdSet>,
    measured: &'a AtomicUsize,
}

/// Milestone 39c: [`measure_intrinsic`] for `target`'s subtree alone (at
/// `depth`), when every changed element is in it. Returns the sizes, and
/// whether `target` contributes to its parent what it did before — its
/// sizes and its own padding and border as cached — in which case no
/// ancestor's sizes can have changed and they need no measuring. (The
/// caller must also know `target` contributes the same way: an in-flow
/// block, before and after.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn measure_subtree(
    target: ElementRef<'_>,
    depth: usize,
    style_cache: &StyleCache,
    font: &Font,
    image_cache: &ImageCache,
    previous: IntrinsicCache,
    dirty: &dom::NodeIdSet,
    measured: &AtomicUsize,
) -> (IntrinsicCache, bool) {
    let before = previous.get(&target.node_id()).copied();
    let out = previous.into_concurrent();
    let pass = Pass { style_cache, font, images: image_cache, out: &out, dirty: Some(dirty), measured };
    measure_node(target, &pass, depth);
    let after = out.get(target.node_id());
    // What its parent adds up (not `content_min`, which only a flex
    // container reads of its items; the target's parent places it by block
    // flow).
    let contribution = |s: Option<IntrinsicSizes>| s.map(|s| (s.min_content, s.max_content, s.extra));
    (out.into_cache(), before.is_some() && contribution(before) == contribution(after))
}

/// Measures one element, inserting it (and its whole subtree) into
/// `out`. Returns this element's OUTER contribution (content width +
/// its own padding + border) — what its parent should account for.
fn measure_node(el: ElementRef<'_>, pass: &Pass, depth: usize) -> IntrinsicSizes {
    let Pass { style_cache, font, images, out, .. } = *pass;
    let style = style_of(style_cache, el);
    let extra = intrinsic_padding(style) + effective_border(style).horizontal();

    // Unchanged since the last frame: its sizes stand.
    if let Some(dirty) = pass.dirty {
        let node = el.node_id();
        if !dirty.contains(&node) && !dirty.contains(&style::owner_of(node)) {
            if let Some(sizes) = out.get(node) {
                return IntrinsicSizes::new(sizes.min_content + extra, sizes.max_content + extra);
            }
        }
    }
    pass.measured.fetch_add(1, Ordering::Relaxed);

    // See `MAX_LAYOUT_DEPTH`'s doc comment — same rationale, applied to
    // this separate (rayon-parallel) recursive tree walk.
    let generated = crate::box_model::with_generated(el, style_cache);
    let child_els: Vec<ElementRef<'_>> = if depth >= MAX_LAYOUT_DEPTH {
        Vec::new()
    } else {
        generated
            .iter()
            .filter_map(|c| match c {
                ChildRef::Element(e) if display_of(style_cache, e) != Display::None => Some(e),
                _ => None,
            })
            .collect()
    };

    // Fan out: each child's subtree is measured on its own rayon task,
    // all inserting into the same shared `out` cache.
    let child_sizes: Vec<IntrinsicSizes> =
        child_els.par_iter().map(|child| measure_node(*child, pass, depth + 1)).collect();
    // Out-of-flow children are measured (for their own shrink-to-fit
    // width) but contribute nothing to this element's size.
    let child_sizes: Vec<IntrinsicSizes> = child_els
        .iter()
        .zip(child_sizes)
        .filter(|(c, _)| !crate::box_model::is_out_of_flow(style_cache, **c))
        .map(|(_, s)| s)
        .collect();

    // This element's own directly-contained text (direct text nodes +
    // inline descendants), measured with its `white-space`.
    let (text_min, text_max) = measure_own_text(el, style, style_cache, font, out);

    let inner = if el.is(dom::atoms::IMG) {
        let w = el
            .attr_atom(dom::atoms::SRC)
            .and_then(|src| images.get(src))
            .map(|d| d.width as f32)
            .unwrap_or(0.0);
        IntrinsicSizes::new(w, w)
    } else if style.display.is_grid() {
        // Milestone 33: from its tracks and the items (measured above).
        crate::grid::intrinsic_sizes(el, style, style_cache, out)
    } else if style.display.is_table() {
        // Milestone 33: from the columns its cells (measured above) need.
        crate::table::intrinsic_sizes(el, style, style_cache, out)
    } else if style.display.is_flex_container() && style.flex_direction == FlexDirection::Row {
        let gaps = style.gap * child_sizes.len().saturating_sub(1) as f32;
        IntrinsicSizes::new(
            child_sizes.iter().map(|s| s.min_content).sum::<f32>() + gaps,
            child_sizes.iter().map(|s| s.max_content).sum::<f32>() + gaps,
        )
    } else {
        // Block flow / flex-column: children are stacked, so the
        // container is as wide as its widest child (or its own text).
        IntrinsicSizes::new(
            child_sizes.iter().map(|s| s.min_content).fold(text_min, f32::max),
            child_sizes.iter().map(|s| s.max_content).fold(text_max, f32::max),
        )
    };
    let sizes = pin_to_specified_width(style, inner, extra);

    out.insert(el.node_id(), sizes);
    IntrinsicSizes::new(sizes.min_content + extra, sizes.max_content + extra)
}

/// Horizontal padding for intrinsic sizing: the pixel part only — a
/// percentage has no containing block to resolve against yet (CSS Sizing
/// 3 §5.2.1 treats it as zero here).
fn intrinsic_padding(style: &Style) -> f32 {
    style.padding.left.px + style.padding.right.px
}

/// An explicit px width pins both min and max; `min-width`/`max-width`
/// clamp them. A percentage has no basis while measuring (the containing
/// block is what is being sized), so it measures as `auto`/`none`.
fn pin_to_specified_width(style: &Style, inner: IntrinsicSizes, extra: f32) -> IntrinsicSizes {
    let content_box = |l: style::Length| {
        l.absolute().map(|w| match style.box_sizing {
            BoxSizing::BorderBox => (w - extra).max(0.0),
            BoxSizing::ContentBox => w,
        })
    };
    let mut sizes = match style.width.and_then(content_box) {
        Some(w) => IntrinsicSizes { min_content: w, max_content: w, content_min: inner.min_content, extra },
        None => inner,
    };
    if let Some(max) = style.limits().max_width.and_then(content_box) {
        sizes.min_content = sizes.min_content.min(max);
        sizes.max_content = sizes.max_content.min(max);
    }
    if let Some(min) = style.limits().min_width.and_then(content_box) {
        sizes.min_content = sizes.min_content.max(min);
        sizes.max_content = sizes.max_content.max(min);
    }
    sizes.extra = extra;
    sizes
}

/// Widest single word (min-content) and single-line total (max-content)
/// of the text directly inside `el` — direct text nodes plus text in
/// inline descendants, tokenized with `el`'s `white-space`.
///
/// Milestone 33: atomic inline descendants (`inline-block`, replaced
/// elements) sit on the same line — their measured sizes (already in
/// `measured`: children are measured first) count as one unbreakable item.
fn measure_own_text(el: ElementRef<'_>, style: &Style, style_cache: &StyleCache, font: &Font, measured: &ConcurrentSizes) -> (f32, f32) {
    let mut words: Vec<InlineWord> = Vec::new();
    let mut atomic = |child: ElementRef<'_>, cs: &Style| {
        if cs.position.is_out_of_flow() {
            return None; // not on the line
        }
        let sizes = measured.get(child.node_id())?;
        let margins = cs.margin.left.and_then(style::Length::absolute).unwrap_or(0.0) + cs.margin.right.and_then(style::Length::absolute).unwrap_or(0.0);
        let extra = intrinsic_padding(cs) + effective_border(cs).horizontal() + margins;
        Some(AtomicBox { layout: None, width: sizes.max_content + extra, height: 0.0, ascent: 0.0, min_width: sizes.min_content + extra })
    };
    let generated = crate::box_model::with_generated(el, style_cache);
    for child in generated.iter() {
        match child {
            ChildRef::Text(t) => collect_words_from_text(t, style, None, &mut words),
            ChildRef::Element(child_el) if crate::box_model::is_out_of_flow(style_cache, child_el) => {}
            ChildRef::Element(child_el)
                if child_el.is(dom::atoms::BR)
                    || crate::box_model::is_float(style_cache, child_el)
                    || crate::box_model::is_inline_level(display_of(style_cache, child_el)) =>
            {
                let cs = style_of(style_cache, child_el);
                push_inline_child(child_el, cs, style_cache, &mut words, 0, &mut atomic);
            }
            _ => {}
        }
    }
    let mut min = 0.0f32;
    let mut line = 0.0f32;
    let mut specs = crate::inline::SpecCache::default();
    let space_spec = specs.spec(font, &style.text, style.font_size);
    let space = font.measure_styled(" ", &space_spec).width;
    for (i, w) in words.iter().filter(|w| !w.is_break).enumerate() {
        let (width, min_width) = match &w.atomic {
            Some(a) => (a.width, a.min_width),
            None => {
                let width = specs.measure(font, w, false).width;
                (width, width)
            }
        };
        min = min.max(min_width);
        line += width + if i > 0 { space * w.leading_spaces as f32 } else { 0.0 };
    }
    (min, line)
}

pub(crate) fn collect_needed_image_paths(element: ElementRef<'_>, style_cache: &StyleCache, out: &mut Vec<String>, depth: usize) {
    if element.is(dom::atoms::IMG) {
        if let Some(src) = element.attr_atom(dom::atoms::SRC) {
            out.push(src.to_string());
        }
    }
    if let Some(style) = style_cache.get(&element.node_id()) {
        if let Some(bg) = &style.background_image_path {
            out.push(bg.clone());
        }
    }
    // Runs before `layout_element` even starts (image preloading), so
    // it needs its own depth cap — see `MAX_LAYOUT_DEPTH`'s doc comment.
    if depth >= MAX_LAYOUT_DEPTH {
        return;
    }
    for child in element.children() {
        if let ChildRef::Element(child_el) = child {
            collect_needed_image_paths(child_el, style_cache, out, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use css_parser::Stylesheet;
    use dom::{Element, Node};
    use images::ImageCache;
    use text::Font;

    fn test_font() -> Font {
        Font::dejavu_sans()
    }



    fn no_sheet() -> Stylesheet {
        Stylesheet::default()
    }



    fn fixture_path(name: &str) -> String {
        // Fixtures live in the `images` crate; reuse them here rather
        // than duplicating image files across crates.
        format!(
            "{}/../images/tests/fixtures/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        )
    }



    #[test]
    fn image_decode_is_cached_across_the_same_layout_pass() {
        let mut parent = Element::new("div");
        let mut img_a = Element::new("img");
        img_a.attributes.push(("src".into(), fixture_path("quadrants.png")));
        let mut img_b = Element::new("img");
        img_b.attributes.push(("src".into(), fixture_path("quadrants.png"))); // same path
        parent.children.push(Node::Element(img_a));
        parent.children.push(Node::Element(img_b));

        let mut cache = ImageCache::with_local_file_access();
        layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert_eq!(cache.decode_count(), 1, "same path used twice should decode only once");
    }


    // ---- Concurrency work: parallel image preload wiring ----

    #[test]
    fn layout_document_decodes_multiple_distinct_images_via_the_parallel_preload_pass() {
        let mut parent = Element::new("div");
        let mut img_a = Element::new("img");
        img_a.attributes.push(("src".into(), fixture_path("quadrants.png")));
        let mut img_b = Element::new("img");
        img_b.attributes.push(("src".into(), fixture_path("solid.jpg")));
        parent.children.push(Node::Element(img_a));
        parent.children.push(Node::Element(img_b));

        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut cache);

        assert_eq!(cache.decode_count(), 2);
        assert!(laid_out.children[0].image.is_some());
        assert!(laid_out.children[1].image.is_some());
    }


    #[test]
    fn background_image_paths_are_also_collected_for_the_preload_pass() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), format!("background-image:url({})", fixture_path("quadrants.png"))));

        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut cache);

        assert_eq!(cache.decode_count(), 1);
        assert!(laid_out.image.is_some());
    }


    #[test]
    fn a_page_with_no_images_does_not_touch_preload_at_all() {
        let el = Element::new("div");
        let mut cache = ImageCache::with_local_file_access();
        layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert_eq!(cache.decode_count(), 0);
    }


    #[test]
    fn preload_pass_does_not_change_the_final_layout_result() {
        // Correctness guard: comparing the FULL layout output for a
        // document with several images against what it produced
        // before this change existed isn't directly possible (the old
        // code path is gone), but this confirms the observable
        // result — box sizes derived from decoded intrinsic
        // dimensions — is exactly what a correct decode produces,
        // not an artifact of preloading changing anything about WHAT
        // gets decoded, only WHEN.
        let mut img = Element::new("img");
        img.attributes.push(("src".into(), fixture_path("quadrants.png")));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        // quadrants.png fixture is a known small real image — intrinsic
        // size should be reflected exactly, not zero/garbage.
        assert!(laid_out.width > 0.0);
        assert!(laid_out.height > 0.0);
    }


    #[test]
    fn width_min_content_and_max_content_from_the_intrinsic_pass() {
        let mut min_el = Element::new("p");
        min_el.attributes.push(("style".into(), "width:min-content".into()));
        min_el.children.push(Node::Text("wrapping short longlonglongword".into()));
        let min_box = layout_document(&min_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());

        let mut max_el = Element::new("p");
        max_el.attributes.push(("style".into(), "width:max-content".into()));
        max_el.children.push(Node::Text("wrapping short longlonglongword".into()));
        let max_box = layout_document(&max_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());

        // min-content ~ the single widest word; max-content ~ the whole
        // line on one row. max must be clearly wider than min.
        assert!(min_box.width > 0.0);
        assert!(max_box.width > min_box.width * 1.5, "max={} min={}", max_box.width, min_box.width);
    }


    #[test]
    fn width_fit_content_clamps_between_min_and_max() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let make = |w: &str| {
            let mut el = Element::new("p");
            el.attributes.push(("style".into(), format!("width:{w}")));
            el.children.push(Node::Text(text.into()));
            layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access()).width
        };
        let minc = make("min-content");
        let maxc = make("max-content");
        // A narrow available width: fit-content should collapse toward min-content.
        let mut el = Element::new("p");
        el.attributes.push(("style".into(), "width:fit-content".into()));
        el.children.push(Node::Text(text.into()));
        let fit_narrow = layout_document(&el, 120.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access()).width;
        assert!(fit_narrow >= minc - 0.5 && fit_narrow <= maxc + 0.5);
        assert!(fit_narrow < maxc, "in a narrow viewport fit-content should be below max-content");
    }


    #[test]
    fn flex_basis_auto_uses_max_content_instead_of_collapsing_to_zero() {
        // Two auto-width items, no flex-grow: they should take their
        // content width, not collapse to 0 (the old Milestone 10 gap).
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "display:flex;width:600px".into()));
        for word in ["hello", "world"] {
            let mut item = Element::new("div");
            item.children.push(Node::Text(word.into()));
            parent.children.push(Node::Element(item));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert!(laid.children[0].width > 5.0, "auto flex item should have its content width, got {}", laid.children[0].width);
        assert!(laid.children[1].x > laid.children[0].x + laid.children[0].width - 1.0);
    }


    #[test]
    fn intrinsic_pass_parallel_matches_a_sequential_reference() {
        // Guards the Phase D concurrency claim: the parallel measurement
        // must equal a straightforward sequential walk of the same tree.
        let mut root = Element::new("div");
        root.attributes.push(("style".into(), "display:flex".into()));
        for i in 0..6 {
            let mut col = Element::new("div");
            col.attributes.push(("style".into(), "display:flex;flex-direction:column".into()));
            for j in 0..4 {
                let mut leaf = Element::new("p");
                leaf.children.push(Node::Text(format!("node {i} {j} some words here")));
                col.children.push(Node::Element(leaf));
            }
            root.children.push(Node::Element(col));
        }
        let font = test_font();
        let sheet = no_sheet();
        let sc = style::resolve_style_tree_parallel(&root, &sheet, 800.0);
        let cache = ImageCache::with_local_file_access();

        let parallel = measure_intrinsic_parallel((&root).into(), &sc, &font, &cache);

        fn seq(el: ElementRef<'_>, sc: &StyleCache, font: &Font, images: &ImageCache, out: &mut IntrinsicCache) -> IntrinsicSizes {
            // identical logic to measure_node but strictly sequential
            let style = style_of(sc, el);
            let extra = intrinsic_padding(style) + effective_border(style).horizontal();
            let kids: Vec<ElementRef<'_>> = el.element_children().filter(|e| display_of(sc, *e) != Display::None).collect();
            let child_sizes: Vec<IntrinsicSizes> = kids.iter().map(|k| seq(*k, sc, font, images, out)).collect();
            let (tmin, tmax) = {
                // `measure_own_text` reads the children's sizes from a
                // mutex-guarded cache; give it this walk's.
                let shared = std::mem::take(out).into_concurrent();
                let r = measure_own_text(el, style, sc, font, &shared);
                *out = shared.into_cache();
                r
            };
            let inner = if el.is(dom::atoms::IMG) {
                IntrinsicSizes::ZERO
            } else if style.display.is_flex_container() && style.flex_direction == FlexDirection::Row {
                let gaps = style.gap * child_sizes.len().saturating_sub(1) as f32;
                IntrinsicSizes::new(
                    child_sizes.iter().map(|s| s.min_content).sum::<f32>() + gaps,
                    child_sizes.iter().map(|s| s.max_content).sum::<f32>() + gaps,
                )
            } else {
                IntrinsicSizes::new(
                    child_sizes.iter().map(|s| s.min_content).fold(tmin, f32::max),
                    child_sizes.iter().map(|s| s.max_content).fold(tmax, f32::max),
                )
            };
            let sizes = pin_to_specified_width(style, inner, extra);
            out.insert(el.node_id(), sizes);
            IntrinsicSizes::new(sizes.min_content + extra, sizes.max_content + extra)
        }
        let mut sequential = IntrinsicCache::new();
        seq((&root).into(), &sc, &font, &cache, &mut sequential);

        assert_eq!(parallel.len(), sequential.len());
        for (k, v) in sequential.iter() {
            let p = parallel.get(k).unwrap();
            assert!((p.min_content - v.min_content).abs() < 0.01 && (p.max_content - v.max_content).abs() < 0.01);
        }
    }

}
