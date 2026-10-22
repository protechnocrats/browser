//! Building one frame: cascade -> transitions -> animations -> layout ->
//! display list -> scene, plus the input/caret/focus helpers that share
//! the laid-out tree.
//!
//! Extracted from `main.rs`'s `build_scene` closure (Milestone 27) so a
//! frame can be built — and therefore traced and tested — without a
//! window or GPU. The code is unchanged apart from the per-stage
//! `tracing` spans and metrics; it is also the direction Milestone 29
//! (the `engine` facade crate) takes this whole file.
//!
//! Every stage runs inside a `diagnostics` span (`frame` > `cascade`,
//! `transitions`, `animations`, `layout`, `focus_outline`,
//! `display_list`, `caret`, `scene_snapshot`). With no subscriber
//! installed those spans are no-ops; `app --trace` installs a collector
//! and prints them per frame.

use crate::page::Page;
use diagnostics::{metrics, tracing::info_span};

/// Milestone 19: finds the laid-out box for `target_key`
/// (`dom::element_ptr_key`) anywhere in the tree, to get real geometry (a
/// box's actual `x`/`y`/`width`, not re-derived/approximated) for the
/// focused input. Placed in the document (Milestone 39b). Searches the
/// whole tree: the last resort, when `layout::LayoutReuse::box_of` did not
/// find the box (a click takes its box from the hit test).
pub fn find_box_by_element_key(root: &layout::LayoutBox, target_key: dom::NodeId) -> Option<layout::Placed<'_>> {
    find_placed(layout::Placed::root(root), &|b| b.element_key == Some(target_key))
}

fn find_placed<'a>(node: layout::Placed<'a>, wanted: &dyn Fn(&layout::LayoutBox) -> bool) -> Option<layout::Placed<'a>> {
    if wanted(node.layout) {
        return Some(node);
    }
    node.children().find_map(|child| find_placed(child, wanted))
}

/// Milestone 19: walks `value`'s words (split-whitespace-shaped,
/// matching how `layout::collect_words_from_text` tokenized them —
/// see that crate) alongside `input_box`'s synthesized word
/// `LayoutBox`es (the same boxes real layout already positioned),
/// yielding `(word_box, word_start_char_index, word_chars)` for each.
/// Both `caret_x_for_cursor` and `cursor_index_for_click_x` walk this
/// exact shape, factored out so the whitespace/word-boundary
/// bookkeeping has only one implementation to get right.
pub fn walk_input_words<'a>(
    input_box: &'a layout::LayoutBox,
    value_chars: &'a [char],
) -> impl Iterator<Item = (&'a layout::LayoutBox, usize, &'a [char])> {
    let mut char_idx = 0usize;
    input_box.children.iter().filter(|c| c.text.is_some()).map(move |word_box| {
        while char_idx < value_chars.len() && value_chars[char_idx].is_whitespace() {
            char_idx += 1;
        }
        let start = char_idx;
        let mut len = 0usize;
        while char_idx < value_chars.len() && !value_chars[char_idx].is_whitespace() {
            char_idx += 1;
            len += 1;
        }
        (word_box, start, &value_chars[start..start + len])
    })
}

/// Milestone 19: the x coordinate to paint the caret at for
/// `cursor_char_index` into `value`, anchored on the input's OWN
/// word boxes' real positions (not re-derived from scratch) —
/// measures only the prefix within whichever word the cursor falls
/// in, so it stays correct regardless of the input's own font-size/
/// styling. Deliberate, documented scope boundary: assumes the value
/// renders on ONE line, which is what a real text input does (it
/// scrolls, never wraps) — this engine's input-value-as-inline-content
/// model doesn't itself prevent wrapping for a very long value, so a
/// wrapped value's caret may land on the wrong visual line. Real
/// single-line inputs (the overwhelming common case) are unaffected.
pub fn caret_x_for_cursor(input_box: &layout::LayoutBox, value: &str, cursor_char_index: usize, font: &text::Font) -> f32 {
    let value_chars: Vec<char> = value.chars().collect();
    for (word_box, start, word) in walk_input_words(input_box, &value_chars) {
        if cursor_char_index < start {
            return word_box.x; // cursor sits in the whitespace run before this word
        }
        if cursor_char_index <= start + word.len() {
            let local = cursor_char_index - start;
            let prefix: String = word[..local].iter().collect();
            let font_size = word_box.text.as_ref().expect("filtered to text boxes above").font_size;
            return word_box.x + font.measure(&prefix, font_size).width;
        }
    }
    match input_box.children.iter().filter(|c| c.text.is_some()).next_back() {
        Some(last) => last.x + last.width,
        // Empty value — no word boxes to anchor on. Approximates
        // content_x as just past the border (ignores any CSS
        // `padding`, which isn't stored on `LayoutBox` — a real,
        // narrow, documented gap; the demo's own inputs set none).
        None => input_box.x + input_box.border.as_ref().map(|b| b.left.width).unwrap_or(0.0) + 2.0,
    }
}

/// Milestone 19: the reverse of `caret_x_for_cursor` — given a click's
/// x position, finds the nearest character boundary. Within the
/// clicked word, picks whichever character offset's measured x is
/// closest to `click_x` (real browsers do the same "nearest boundary,
/// not always rounding down" snapping).
pub fn cursor_index_for_click_x(input_box: &layout::LayoutBox, value: &str, click_x: f32, font: &text::Font) -> usize {
    let value_chars: Vec<char> = value.chars().collect();
    for (word_box, start, word) in walk_input_words(input_box, &value_chars) {
        if click_x < word_box.x {
            return start;
        }
        if click_x <= word_box.x + word_box.width {
            let font_size = word_box.text.as_ref().expect("filtered to text boxes above").font_size;
            let mut best_offset = 0usize;
            let mut best_dist = f32::MAX;
            for offset in 0..=word.len() {
                let prefix: String = word[..offset].iter().collect();
                let x = word_box.x + font.measure(&prefix, font_size).width;
                let dist = (x - click_x).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_offset = offset;
                }
            }
            return start + best_offset;
        }
    }
    value_chars.len()
}

/// Milestone 19: the caret's vertical extent (top y, height) — the
/// first word box's real line-box position when the value isn't
/// empty (already exactly where real inline layout put it), or a
/// border-inset approximation of the content area when it is (same
/// caveat as `caret_x_for_cursor`'s empty-value fallback).
pub fn caret_extent(input_box: &layout::LayoutBox) -> (f32, f32) {
    match input_box.children.iter().find(|c| c.text.is_some()) {
        Some(word) => (word.y, word.height),
        None => {
            let (top_w, bottom_w) = input_box.border.as_ref().map_or((0.0, 0.0), |b| (b.top.width, b.bottom.width));
            (input_box.y + top_w, (input_box.height - top_w - bottom_w).max(4.0))
        }
    }
}

/// Milestone 21: resolves a navigation target (an `<a href>`, or a
/// `<form action>` + query string) against the CURRENT page's own URL
/// — `network::resolve_url` handles both relative AND already-absolute
/// references correctly (an absolute reference passed as `relative`
/// just passes through unchanged, per URL-join semantics), so this
/// only needs its OWN special case for when there's no current page
/// URL at all (the hardcoded demo): an already-absolute target still
/// works (nothing to resolve, it's already a real destination); a
/// relative one has no base to resolve against, so `None` (no-op —
/// documented, not a crash or a wrong-looking navigation to garbage).
pub fn resolve_navigation_target(current_url: &Option<String>, action: &str) -> Option<String> {
    match current_url {
        Some(base) => network::resolve_url(base, action),
        None if action.starts_with("http://") || action.starts_with("https://") => Some(action.to_string()),
        None => None,
    }
}

pub fn collect_focusable_ids(element: dom::ElementRef<'_>, out: &mut Vec<String>) {
    let is_focusable = matches!(element.tag(), "input" | "textarea" | "button" | "select") || element.is_text_editable();
    if is_focusable {
        if let Some(id) = element.attr_atom(dom::atoms::ID) {
            out.push(id.to_string());
        }
    }
    for child in element.children() {
        if let dom::ChildRef::Element(el) = child {
            collect_focusable_ids(el, out);
        }
    }
}

/// Counts every box in a laid-out tree. Iterative: the tree can be as
/// deep as the layout depth cap, and this is diagnostic code that must
/// never be the thing that overflows a stack.
fn count_boxes(root: &layout::LayoutBox) -> u64 {
    let mut count = 0u64;
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        count += 1;
        pending.extend(node.children.iter());
    }
    count
}

/// Gives the boxes of the elements in `transforms` their new transform (a
/// compositor frame keeps the layout; a later display list built from it
/// must see where the layers now are). Iterative: layout trees are deep.
fn set_box_transforms(root: &mut layout::LayoutBox, transforms: &std::collections::HashMap<dom::NodeId, style::Transform>) {
    let mut stack = vec![root];
    while let Some(b) = stack.pop() {
        if let Some(t) = b.element_key.and_then(|k| transforms.get(&k)) {
            b.extras_mut().transform = *t;
        }
        stack.extend(b.children.iter_mut());
    }
}

/// What the previous frame of a page was built from and drew: decides
/// whether the next frame can reuse its layout, and is what the next
/// frame's damage is measured against.
pub struct FrameMemo {
    width: f32,
    height: f32,
    background: style::Color,
    /// Shared with the frame's `Scene`, not copied.
    items: std::sync::Arc<Vec<rendering::DisplayItem>>,
    /// The frame's `Scene::sequence`: what the next frame's damage is
    /// relative to.
    sequence: u64,
    /// The page's display list for the layout this frame drew — what the
    /// next frame draws from while the layout is reused.
    page_list: Option<rendering::PageDisplayList>,
    /// The layout draws a `position: sticky` box: frames build their
    /// display list at their scroll offset instead.
    sticky: bool,
}

/// Builds one frame of `page` at `width` x `height` (Milestone 31: doing
/// only the work its changes require — `docs/adr/008-dirty-tracking-and-identity.md`):
///
/// - the document's mutation records drive an incremental restyle
///   (`style::StyleEngine`); transitions and `@keyframes` then touch only
///   the animated elements;
/// - layout runs only if the document, a final style, or the width changed
///   — a scroll, caret or focus change reuses the last layout;
/// - the display list is culled to the viewport, and diffed against the
///   last one for the frame's damage.
///
/// Observability: a `frame` span with a span per stage, `FRAMES_BUILT`, the
/// frame-time histogram, and the returned [`FrameStats`] (per page, per
/// frame — what tests assert on).
///
/// `now` is the frame's clock, **injected by the caller**: transitions and
/// animations interpolate against it, so a fixed `now` makes a frame
/// reproducible (architecture doc §9).
pub fn build_frame(
    page: &Page,
    font: &text::Font,
    atlas: &mut text::GlyphAtlas,
    width: f32,
    height: f32,
    now: std::time::Instant,
) -> (rendering::Scene, crate::FrameStats) {
    // (Diagnostics only: how long building this frame took. Nothing that
    // affects what is drawn reads the wall clock.)
    let frame_started = std::time::Instant::now();
    let _frame = info_span!("frame", width = width as f64, height = height as f64).entered();

    let mutations = page.document.borrow_mut().take_mutations();
    let document = page.document.borrow();
    let sheet = page.stylesheet.borrow();
    let mut styles = page.styles.borrow_mut();
    // M4's gap: what the user is doing, for `:hover`, `:active` and
    // `:focus`; the elements whose state changed are restyled below.
    let focus = page.js_env.borrow().focused_element_id().and_then(|id| document.node_id_of(&id));
    let focus = focus.map(|n| std::iter::once(n).chain(document.ancestors_of(n).iter().map(|a| a.node_id())).collect()).unwrap_or_default();
    styles.set_interaction(style::Interaction { hover: page.hover_path.borrow().clone(), active: page.active_path.borrow().clone(), focus });

    let restyle = {
        let _s = info_span!("cascade").entered();
        styles.restyle(&document, &sheet, page.sheet_generation.get(), style::ViewportSize::new(width, height), &mutations)
    };
    metrics::STYLE_CACHE_ENTRIES.set(styles.len() as u64);
    let animation = if styles.has_animated_elements() {
        let _s = info_span!("animations").entered();
        styles.animate(&mut page.transition_registry.borrow_mut(), &mut page.animation_registry.borrow_mut(), now)
    } else {
        style::AnimationReport::default()
    };
    let animated = &animation.changed;

    let mut previous = page.last_frame.borrow_mut().take();
    // Elements showing an image that just arrived: laid out again.
    let image_dirty = std::mem::take(&mut *page.image_dirty.borrow_mut());
    let layout_valid = page.last_layout.borrow().is_some()
        && previous.as_ref().is_some_and(|p| p.width == width && p.height == height)
        && mutations.is_empty()
        && restyle.changed.is_empty()
        && animated.is_empty()
        && image_dirty.is_empty();
    // Milestone 34: transform-only animation of elements on layers of their
    // own. With the layout standing, the compositor moves their layers (the
    // layout boxes and the page's display list are updated in place); a
    // frame that lays out anyway lays them out with the rest.
    let compositor_transforms: std::collections::HashMap<dom::NodeId, style::Transform> =
        if layout_valid { animation.composited.iter().copied().collect() } else { Default::default() };
    if !compositor_transforms.is_empty() {
        if let Some(root) = page.last_layout.borrow_mut().as_mut() {
            set_box_transforms(root, &compositor_transforms);
        }
    }
    let layout_work = if layout_valid {
        crate::LayoutWork::Reused
    } else {
        let _s = info_span!("layout").entered();
        // Layout-dirty: every mutated element and every element whose final
        // style changed, with all their ancestors; everything else can be
        // moved over from the previous layout (`layout::LayoutReuse`).
        let mut reuse = page.layout_reuse.borrow_mut();
        // The viewport is the containing block of `fixed` boxes (and of
        // `absolute` ones with no positioned ancestor): a new height means
        // a fresh layout (resizes are rare).
        let viewport_changed = previous.as_ref().is_none_or(|p| p.height != height);
        let previous_tree =
            if mutations.overflowed || restyle.full || viewport_changed { None } else { page.last_layout.borrow_mut().take() };
        reuse.set_viewport(width, height);
        let touched: Vec<dom::NodeId> =
            mutations.records.iter().map(|r| r.target).chain(restyle.changed.iter().copied()).chain(animated.iter().copied())
                .chain(animation.composited.iter().map(|(n, _)| *n))
                .chain(image_dirty.iter().copied())
                .collect();
        let dirty = if previous_tree.is_some() {
            let _s = info_span!("layout_dirty_set").entered();
            layout::LayoutReuse::with_ancestors(&document, touched.iter().copied())
        } else {
            Default::default() // nothing to reuse: no dirty set needed
        };
        // Entries are kept until an element is laid out again (ADR 022): drop
        // those of removed elements once they could outnumber the live ones
        // (generated content counts as its element).
        if reuse.tracked() > 2 * document.element_count() + 1024 {
            reuse.retain_live(|n| document.contains(style::owner_of(n)));
        }
        reuse.begin_frame(previous_tree, dirty);
        let laid_out = layout::layout_document_incremental(
            document.root(),
            width,
            font,
            styles.styles(),
            &mut page.image_cache.borrow_mut(),
            &mut reuse,
            &touched,
        );
        // Counting walks the whole tree: only when it is reported.
        let boxes = if reuse.reused == 0 || metrics::detailed() {
            let _s = info_span!("layout_count").entered();
            count_boxes(&laid_out)
        } else {
            0
        };
        if metrics::detailed() {
            metrics::LAYOUT_BOX_COUNT.set(boxes);
        }
        *page.last_layout.borrow_mut() = Some(laid_out);
        if reuse.reused == 0 {
            crate::LayoutWork::Full { boxes: boxes as usize }
        } else {
            crate::LayoutWork::Incremental { laid_out: reuse.laid_out, reused_boxes: reuse.reused }
        }
    };
    let background = canvas_background(&document, styles.styles());
    drop(styles);
    let laid_out_ref = page.last_layout.borrow();
    let Some(laid_out) = laid_out_ref.as_ref() else { unreachable!("a layout was just built or reused") };
    let scroll = page.scroll_offset_y.get();

    // The display list: drawn from the page's list, kept across frames. A
    // frame that reuses the layout draws from it (a scroll, caret or focus
    // frame costs what is near the viewport), building it if there is none;
    // a frame whose layout changed incrementally updates it — the subtrees
    // the layout moved over are moved over, only what was laid out again is
    // built (Milestone 39b, ADR 022). A fresh layout builds for itself, as
    // before (the list's banding would only add to a first frame), and so
    // does a page with a sticky box, each frame at its scroll offset.
    let layout_reused = matches!(layout_work, crate::LayoutWork::Reused);
    let layout_incremental = matches!(layout_work, crate::LayoutWork::Incremental { .. });
    // Only a list whose glyph runs point into this atlas (the page thread
    // gives a new document a new atlas).
    let mut previous_list = previous.as_mut().and_then(|p| p.page_list.take()).filter(|l| l.drawable_with(atlas));
    let mut composited = match previous_list.as_mut() {
        Some(list) if layout_reused && !compositor_transforms.is_empty() => {
            let _s = info_span!("compositor_update").entered();
            list.update_transforms(&compositor_transforms)
        }
        _ => 0,
    };
    let sticky = previous.as_ref().is_some_and(|p| p.sticky);
    let container_scrolls = page.container_scrolls.borrow();
    let mut display_list_built = true;
    let (mut items, image_resources, page_list, sticky, drawn_from) = {
        let _s = info_span!("display_list").entered();
        let list = match previous_list {
            _ if sticky => None,
            Some(mut list) if layout_reused => {
                display_list_built = false;
                // A scroll container scrolled: its layer moves (Milestone 34).
                composited += list.set_container_scrolls(&container_scrolls);
                Some(list)
            }
            Some(mut list) if layout_incremental => {
                // Milestone 39c: a layout patched in place patches the list
                // in place, if it can (ADR 022 §9).
                let patched = page.layout_reuse.borrow().patched().map(|(_, path)| path.to_vec());
                match patched {
                    Some(path) if list.patch(laid_out, &path, font, atlas, &container_scrolls) => Some(list),
                    _ => Some(list.update(laid_out, font, atlas, &container_scrolls)),
                }
            }
            _ if layout_reused || layout_incremental => Some(rendering::PageDisplayList::build(laid_out, font, atlas, &container_scrolls)),
            _ => None,
        };
        if let Some(list) = &list {
            metrics::DISPLAY_BOXES_BUILT.set(list.built_boxes as u64);
        }
        match list {
            Some(list) if !list.has_sticky() => {
                let items = list.visible(scroll, width, height);
                let images = list.images().to_vec();
                let total = list.len();
                (items, images, Some(list), false, total)
            }
            // A sticky box sits where the scroll offset puts it: build at
            // this offset (and keep doing so while the layout stands).
            _ => {
                display_list_built = true;
                let (items, images, saw_sticky) = rendering::build_display_list_for_frame(laid_out, font, atlas, scroll, &container_scrolls);
                let total = items.len();
                (items, images, None, saw_sticky, total)
            }
        }
    };
    drop(container_scrolls);
    let drawn = items.len();

    // The focused element: its focus ring and, if it is editable, the caret.
    // Its box is found once, by its recorded position (O(depth)) — the tree
    // is searched only if no position was recorded.
    let focused_id = page.js_env.borrow().focused_element_id();
    let focused_el = focused_id.as_deref().and_then(|id| document.find_by_id(id));
    // With its words, in document space (Milestone 39b).
    let focused_box = focused_el.and_then(|el| {
        page.layout_reuse.borrow().box_of(laid_out, el.node_id()).or_else(|| find_box_by_element_key(laid_out, el.node_id()))
    });
    let focused_box = focused_box.map(|b| b.with_children_in_document_space());
    let focused_box = focused_box.as_ref();
    if let Some(focused_box) = focused_box {
        // Milestone 16: without a visible focus indicator, Tab/click
        // changing focus is invisible. Painted at display-list time (not by
        // modifying the layout tree, which is reused across frames), and
        // only when the author set no `outline` of their own.
        let _s = info_span!("focus_outline").entered();
        if focused_box.extras().outline.is_none() {
            items.extend(rendering::outline_items(focused_box, &focus_ring(), scroll));
        }
    }
    // Milestone 19: the caret — a paint-time overlay at a position within
    // the focused editable box. No blink (a documented simplification).
    if let (Some(focused_id), Some(el), Some(input_box)) = (focused_id.as_deref(), focused_el, focused_box) {
        if el.is_text_editable() {
            let _s = info_span!("caret").entered();
            let value = el.editable_value();
            let cursor_idx = page.js_env.borrow().cursor_position(focused_id);
            let caret_x = caret_x_for_cursor(input_box, &value, cursor_idx, font);
            let (caret_y, caret_h) = caret_extent(input_box);
            items.push(rendering::DisplayItem::Rect {
                x: caret_x,
                y: caret_y - scroll,
                width: 2.0,
                height: caret_h,
                color: style::Color { r: 205, g: 214, b: 244, a: 255 },
                radius: 0.0,
            });
        }
    }

    // Only what intersects the viewport is drawn (the page's list already
    // left out everything else: that counts as culled too).
    let culled = rendering::cull_to_viewport(&mut items, width, height) + (drawn_from - drawn);
    let items = std::sync::Arc::new(items);
    let (damage, damage_base) = match &previous {
        Some(p) if p.width == width && p.height == height && p.background == background => (rendering::damage_between(&p.items, &items), Some(p.sequence)),
        _ => (rendering::Damage::Full, None),
    };
    let sequence = rendering::Scene::next_sequence();
    let stats = crate::FrameStats {
        restyled: restyle.restyled,
        full_restyle: restyle.full,
        animated: animated.len() + animation.composited.len(),
        composited,
        layout: layout_work,
        display_items: items.len(),
        culled,
        damage,
        display_list_built,
        // Filled in by the page loop, which owns the budget (ADR 020).
        animation_interval_ms: 0,
    };
    *page.last_frame.borrow_mut() = Some(FrameMemo { width, height, background, items: std::sync::Arc::clone(&items), sequence, page_list, sticky });

    let scene = {
        let _s = info_span!("scene_snapshot").entered();
        rendering::Scene {
            viewport_width: width,
            viewport_height: height,
            background,
            items,
            glyph_atlas: rendering::GlyphAtlasSnapshot::from_atlas(atlas),
            images: image_resources,
            damage,
            sequence,
            damage_base,
        }
    };

    metrics::FRAMES_BUILT.inc();
    metrics::record_frame_time(frame_started.elapsed());
    (scene, stats)
}

/// The canvas colour (CSS 2 §14.2): the root element's background colour,
/// or `<body>`'s when the root's is transparent — composited over white,
/// the engine's default canvas, so the result is opaque.
fn canvas_background(document: &dom::Document, styles: &style::StyleCache) -> style::Color {
    let root = document.root();
    let background_of = |el: dom::ElementRef<'_>| styles.get(&el.node_id()).map(|s| s.background_color).filter(|c| c.a > 0);
    let body = root.children().find_map(|n| match n {
        dom::ChildRef::Element(el) if el.is(dom::atoms::BODY) => Some(el),
        _ => None,
    });
    let chosen = background_of(root).or_else(|| body.and_then(background_of));
    match chosen {
        Some(c) => {
            let a = u16::from(c.a);
            let over_white = |v: u8| ((u16::from(v) * a + 255 * (255 - a) + 127) / 255) as u8;
            style::Color { r: over_white(c.r), g: over_white(c.g), b: over_white(c.b), a: 255 }
        }
        None => style::Color { r: 255, g: 255, b: 255, a: 255 },
    }
}

/// The engine's default focus ring (used when the author set no `outline`).
fn focus_ring() -> layout::Outline {
    layout::Outline { width: 2.0, color: style::Color { r: 137, g: 180, b: 250, a: 255 }, style: style::BorderStyle::Solid, offset: 2.0 }
}
