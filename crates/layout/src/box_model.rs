//! The core per-element box-model computation (`layout_element`): margin/
//! border/padding/content sizing, image intrinsic sizing, form-control
//! (checkbox/radio) defaults, and dispatch into either block stacking
//! (inline text + block children, via `crate::inline`) or flex layout
//! (via `crate::flex`) for an element's children.

use crate::inline::{collect_words_from_text, flush_pending_inline, push_inline_child, AtomicBox, InlineWord};
use crate::intrinsic::{IntrinsicCache, IntrinsicSizes};
use crate::{Border, LayoutBox, Outline, MAX_LAYOUT_DEPTH};
use dom::{ChildRef, Element, ElementRef, Node};
use images::ImageCache;
use std::sync::Arc;
use style::{BorderStyle, BoxSizing, Display, EdgeSizes, Style, StyleCache};
use text::Font;

/// Looks up an element's precomputed style. Every element passed to
/// `layout_element` (and its helpers) was necessarily visited by
/// `resolve_style_tree_parallel`'s walk first (it walks the exact
/// same tree, unconditionally), so a miss here would mean the two
/// walks have gone out of sync with each other — falling back to
/// `Style::default()` rather than panicking keeps that latent-bug
/// case a wrong-looking render instead of a crash, consistent with
/// this project's graceful-degradation discipline elsewhere.
pub(crate) fn style_of<'a>(style_cache: &'a StyleCache, element: ElementRef<'_>) -> &'a Style {
    style_ref(style_cache, element)
}

/// An element's style by reference — the cache does not change during
/// layout, and not copying a whole `Style` into every level of the
/// recursive layout keeps its stack frames small (the deep-tree stress
/// inputs of the fuzz suite).
pub(crate) fn style_ref<'a>(style_cache: &'a StyleCache, element: ElementRef<'_>) -> &'a Style {
    static DEFAULT: std::sync::OnceLock<Style> = std::sync::OnceLock::new();
    style_cache.get(&element.node_id()).unwrap_or_else(|| DEFAULT.get_or_init(Style::default))
}

/// Reads just an element's margin without cloning its whole `Style`
/// (which carries a `HashMap` of custom properties, a `Vec` of
/// transitions, and more — real allocation-bearing state, not a cheap
/// `Copy`). `EdgeSizes` itself IS `Copy`, so this is a plain field read
/// off the cached entry, not a clone. Performance fix: the call site in
/// the block-children loop used to do `style_of(...).margin` right
/// before calling `layout_element` on the SAME child, which calls
/// `style_of` again internally — two full `Style` clones back-to-back
/// for one element, every restyle.
pub(crate) fn margin_of(style_cache: &StyleCache, element: ElementRef<'_>, containing_width: f32) -> EdgeSizes {
    style_cache.get(&element.node_id()).map(|s| s.margin.resolve(containing_width)).unwrap_or_default()
}

/// Each side's used border width (M6's gap: sides differ). Every visible
/// style reserves the same space as `solid`.
#[inline]
pub(crate) fn effective_border(style: &Style) -> EdgeSizes {
    style.border_widths()
}

/// Decodes `style.background_image_path` (if set) via the cache.
/// A missing file or bad path/decode error just yields `None` — a
/// broken image degrades to "nothing painted there," not a crash or
/// a broken-image icon placeholder (no icon asset exists to show).
fn resolve_background_image(style: &Style, image_cache: &mut ImageCache) -> Option<Arc<images::DecodedImage>> {
    let path = style.background_image_path.as_ref()?;
    image_cache.load_from_file(path).ok()
}

/// `<img>`'s intrinsic size, if it has a decodable `src`. `None` for
/// any non-`<img>` element, a missing `src`, or a decode failure.
fn resolve_img_intrinsic_size(
    element: ElementRef<'_>,
    image_cache: &mut ImageCache,
) -> Option<(f32, f32, Arc<images::DecodedImage>)> {
    if element.tag() != "img" {
        return None;
    }
    let src = element.attr_atom(dom::atoms::SRC)?;
    let decoded = image_cache.load_from_file(src).ok()?;
    Some((decoded.width as f32, decoded.height as f32, decoded))
}

/// Clamps a border-box size between `min` and `max` (`max` first, so a
/// `min` larger than `max` wins — CSS 2 §10.4), each converted to a
/// border-box size by `box-sizing`.
pub(crate) fn clamp_to_min_max(size: f32, min: Option<style::Length>, max: Option<style::Length>, basis: f32, sizing: BoxSizing, extra: f32) -> f32 {
    let border_box = |l: style::Length| {
        let v = l.resolve(basis).max(0.0);
        match sizing {
            BoxSizing::BorderBox => v,
            BoxSizing::ContentBox => v + extra,
        }
    };
    let mut size = size;
    if let Some(max) = max {
        size = size.min(border_box(max));
    }
    if let Some(min) = min {
        size = size.max(border_box(min));
    }
    size
}

/// Milestone 33: an element's children as layout sees them — its
/// `::before` and `::after`, when they generate a box, as synthetic
/// elements (whose styles the cascade stored under their pseudo ids)
/// around its real children.
pub(crate) struct WithGenerated<'a> {
    before: Option<Node>,
    element: ElementRef<'a>,
    after: Option<Node>,
}

impl<'a> WithGenerated<'a> {
    pub(crate) fn iter(&self) -> impl Iterator<Item = ChildRef<'_>> {
        self.before.iter().map(ChildRef::from).chain(self.element.children()).chain(self.after.iter().map(ChildRef::from))
    }
}

pub(crate) fn with_generated<'a>(element: ElementRef<'a>, style_cache: &StyleCache) -> WithGenerated<'a> {
    // One lookup of the element's own style says whether there is any.
    let owner = style_cache.get(&element.node_id());
    let generated = |which: style::PseudoElement| {
        // Generated content does not generate content of its own.
        if style::is_pseudo(element.node_id()) {
            return None;
        }
        let exists = owner.is_some_and(|s| match which {
            style::PseudoElement::Before => s.generates_before,
            style::PseudoElement::After => s.generates_after,
        });
        if !exists {
            return None;
        }
        let id = style::pseudo_id(element.node_id(), which);
        let text = style_cache.get(&id)?.content.clone()?;
        let mut el = Element::new(match which {
            style::PseudoElement::Before => "::before",
            style::PseudoElement::After => "::after",
        });
        el.node_id = id;
        if !text.is_empty() {
            el.children.push(Node::Text(text));
        }
        Some(Node::Element(el))
    };
    WithGenerated { before: generated(style::PseudoElement::Before), element, after: generated(style::PseudoElement::After) }
}

/// An element's computed `display`, without cloning its style.
pub(crate) fn display_of(style_cache: &StyleCache, element: ElementRef<'_>) -> Display {
    style_cache.get(&element.node_id()).map_or(Display::Block, |s| s.display)
}

/// Whether a box of this `display` goes on its parent's lines.
pub(crate) fn is_inline_level(display: Display) -> bool {
    display == Display::Inline || display.is_atomic_inline()
}

/// Lays out an atomic inline box (margin box at the origin) for line
/// layout to place. Never linked into [`crate::LayoutReuse`]'s tree, so
/// it is laid out fresh each frame (like a flex item).
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_atomic(
    element: ElementRef<'_>,
    style: &Style,
    containing_width: f32,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> Option<AtomicBox> {
    if depth >= MAX_LAYOUT_DEPTH {
        diagnostics::guard(diagnostics::Guard::LayoutDepth, "inline-block past the layout depth cap; skipped");
        return None;
    }
    let margin = style.margin.resolve(containing_width);
    let laid_out = layout_element(element, 0.0, 0.0, containing_width, font, style_cache, intrinsic, image_cache, reuse, depth);
    Some(AtomicBox::from_layout(laid_out, margin))
}

/// The default size of a replaced element or form control with no CSS
/// size (HTML §15.5; CSS's 300×150 default object size): text inputs are
/// about 20 characters wide and one line high, a textarea two lines,
/// embedded content 300×150. `None`: sized by its content instead.
fn replaced_default_size(element: ElementRef<'_>, style: &Style, font: &Font) -> Option<(f32, f32)> {
    let line = font.measure("x", style.font_size).line_height;
    match element.tag() {
        "input" => match element.attr_atom(dom::atoms::TYPE).map(str::to_ascii_lowercase).as_deref() {
            Some("submit" | "reset" | "button" | "image" | "file" | "color" | "range") => None,
            _ => Some((150.0, line)),
        },
        "textarea" => Some((180.0, line * 2.0)),
        "canvas" | "video" | "iframe" | "embed" | "object" | "svg" => Some((300.0, 150.0)),
        _ => None,
    }
}

/// Two adjoining vertical margins collapse to the largest positive one plus
/// the most negative one (CSS 2 §8.3.1).
pub(crate) fn collapse_margins(a: f32, b: f32) -> f32 {
    a.max(b).max(0.0) + a.min(b).min(0.0)
}

/// The element's `height` in pixels, when it has one. A percentage height
/// needs a containing block of definite height; block layout here does not
/// yet track one, so a percentage behaves as `auto` — which is also what
/// CSS says for the common case of a containing block sized by its
/// content (CSS 2 §10.5).
pub(crate) fn definite_height(style: &Style) -> Option<f32> {
    style.height.and_then(style::Length::absolute).map(|h| h.max(0.0))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_element(
    element: ElementRef<'_>,
    outer_x: f32,
    outer_y: f32,
    available_width: f32,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> LayoutBox {
    // Milestone 31: an unchanged subtree at the same width is the previous
    // frame's layout, moved (see `reuse.rs`).
    if let Some(reused) = reuse.take(element, outer_x, outer_y, available_width, depth) {
        return reused;
    }
    layout_element_sized(element, outer_x, outer_y, available_width, None, None, font, style_cache, intrinsic, image_cache, reuse, depth)
}

/// [`layout_element`] with its border-box width and/or height decided by
/// the caller (`forced_width`, `forced_height`) — a flex item sized by its
/// container, an absolutely positioned box sized by its insets. `None`: the
/// element's own rules. Never takes a reused box.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_element_sized(
    element: ElementRef<'_>,
    outer_x: f32,
    outer_y: f32,
    available_width: f32,
    forced_width: Option<f32>,
    forced_height: Option<f32>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> LayoutBox {
    let style = style_ref(style_cache, element);
    // Milestone 33: a box that establishes a block formatting context
    // starts with no floats to flow around; any other block container
    // shares its ancestors' (and drops its own when done).
    let saved_exclusions = establishes_formatting_context(style).then(|| std::mem::take(&mut reuse.exclusions));
    let exclusions_at_start = reuse.exclusions.len();
    // Milestone 33: percentages in margins and padding are of the
    // containing block's width, on every side.
    let mut margin = style.margin.resolve(available_width);
    let padding = style.padding.resolve(available_width);
    let border_width = effective_border(style);
    let edge_extra = padding.left + padding.right + border_width.horizontal();

    let img_intrinsic = resolve_img_intrinsic_size(element, image_cache);
    let background_image = resolve_background_image(style, image_cache);
    // An <img>'s own decoded image IS its content — takes priority
    // over any (unusual, but not invalid) background-image also set
    // on the same element.
    let image = img_intrinsic.as_ref().map(|(_, _, arc)| Arc::clone(arc)).or(background_image);

    // Milestone 19: `<input type="checkbox">`/`<input type="radio">`
    // get a small fixed default size (16x16, matching a typical real
    // UA stylesheet default) unless CSS explicitly overrides — without
    // this they'd fall through to the ordinary "no width set" default,
    // which is the FULL container width (correct for a text input,
    // absurd for a checkbox). Modeled on `img_intrinsic`'s exact same
    // "used only when the author didn't set an explicit size" shape.
    let is_checkbox_or_radio = element.is(dom::atoms::INPUT) && matches!(element.attr_atom(dom::atoms::TYPE), Some("checkbox") | Some("radio"));
    let control_intrinsic: Option<(f32, f32)> = if is_checkbox_or_radio {
        Some((16.0, 16.0))
    } else {
        replaced_default_size(element, style, font)
    };
    // Milestone 33: an image with one dimension set keeps its aspect ratio.
    let img_ratio = img_intrinsic.as_ref().filter(|(w, h, _)| *w > 0.0 && *h > 0.0).map(|(w, h, _)| h / w);

    // An atomic inline box with `width: auto` is sized to fit its content
    // (CSS 2 §10.3.9) — unless it is a replaced element, which has a size.
    // So is an out-of-flow box whose insets do not decide its width
    // (§10.3.7).
    let atomic = style.display.is_atomic_inline();
    let out_of_flow = style.position.is_out_of_flow();
    let floated = style.float != style::Float::None && !out_of_flow;
    // A table is as wide as its columns need, up to its container.
    let shrink_to_fit = atomic || floated || style.display.is_table() || (out_of_flow && !(style.inset().left.is_some() && style.inset().right.is_some()));
    let width_keyword = style.width_intrinsic.or_else(|| {
        (shrink_to_fit && style.width.is_none() && img_intrinsic.is_none() && control_intrinsic.is_none())
            .then_some(style::IntrinsicKeyword::FitContent)
    });

    let border_box_width = if let Some(kw) = width_keyword {
        // Phase D: `width: min-content | max-content | fit-content`.
        let sizes = intrinsic.get(&element.node_id()).copied().unwrap_or(IntrinsicSizes::ZERO);
        let avail_content = (available_width - margin.left - margin.right - edge_extra).max(0.0);
        let content_w = match kw {
            style::IntrinsicKeyword::MinContent => sizes.min_content,
            style::IntrinsicKeyword::MaxContent => sizes.max_content,
            style::IntrinsicKeyword::FitContent => sizes.max_content.min(avail_content).max(sizes.min_content),
        };
        content_w + edge_extra
    } else {
        match style.width {
            Some(w) => match style.box_sizing {
                BoxSizing::BorderBox => w.resolve(available_width).max(0.0),
                BoxSizing::ContentBox => w.resolve(available_width).max(0.0) + edge_extra,
            },
            None => match (&img_intrinsic, control_intrinsic) {
                (Some((w, _, _)), _) => match (definite_height(style), img_ratio) {
                    (Some(h), Some(ratio)) if style.box_sizing == BoxSizing::ContentBox => h / ratio + edge_extra,
                    _ => w + edge_extra,
                },
                (None, Some((w, _))) => w + edge_extra,
                (None, None) => (available_width - margin.left - margin.right).max(0.0),
            },
        }
    };
    // Milestone 33: `min-width` / `max-width` (CSS 2 §10.4) — unless the
    // caller already decided the width.
    let unclamped_width = border_box_width;
    let border_box_width = match forced_width {
        Some(w) => w.max(0.0),
        None => clamp_to_min_max(border_box_width, style.limits().min_width, style.limits().max_width, available_width, style.box_sizing, edge_extra),
    };
    // A width `max-width`/`min-width` changed is as definite as a set one
    // (so `max-width: 960px; margin: 0 auto` centres).
    let width_was_constrained = forced_width.is_none() && border_box_width != unclamped_width;
    let content_width = (border_box_width - padding.left - padding.right - border_width.horizontal()).max(0.0);

    // CSS 2 §10.3.3: `auto` horizontal margins take up the space a box
    // with a definite width leaves — both `auto` centres it. (A box whose
    // width is `auto` fills the line, so its `auto` margins are zero.)
    // Only a block-level box: an atomic inline's `auto` margins are zero.
    if forced_width.is_none()
        && !atomic
        && !out_of_flow
        && !floated
        && (style.width.is_some() || style.width_intrinsic.is_some() || width_was_constrained || img_intrinsic.is_some() || control_intrinsic.is_some())
    {
        let free = available_width - border_box_width - margin.left - margin.right;
        match (style.margin.left.is_none(), style.margin.right.is_none()) {
            (true, true) => margin.left = (free / 2.0).max(0.0),
            (true, false) => margin.left = free.max(0.0),
            _ => {}
        }
    }

    let border_box_x = outer_x + margin.left;
    let border_box_y = outer_y + margin.top;
    let content_x = border_box_x + border_width.left + padding.left;
    let content_start_y = border_box_y + border_width.top + padding.top;

    let cursor_y;
    let children;

    if style.display.is_flex_container() {
        // Milestone 10: flex container — a genuinely different
        // algorithm from block stacking, see `layout_flex_children`'s
        // own doc comment for exact scope (row direction has full
        // grow/shrink distribution; column direction has correct
        // stacking/justify/align but does NOT resize items via grow/
        // shrink — a documented, deliberate scope reduction).
        let explicit_content_height = definite_height(style).map(|h| match style.box_sizing {
            BoxSizing::BorderBox => (h - padding.top - padding.bottom - border_width.vertical()).max(0.0),
            BoxSizing::ContentBox => h,
        });
        let (flex_boxes, used_height) = if depth >= MAX_LAYOUT_DEPTH {
            diagnostics::guard(diagnostics::Guard::LayoutDepth, "flex container past the layout depth cap; children skipped");
            (Vec::new(), 0.0)
        } else {
            // Out-of-flow children are not flex items; each would have
            // been at the start of the container's content box.
            for child in element.children().filter_map(|c| match c {
                ChildRef::Element(e) if is_out_of_flow(style_cache, e) => Some(e),
                _ => None,
            }) {
                reuse.record_static_position(child.node_id(), element.node_id(), content_x - border_box_x, content_start_y - border_box_y);
            }
            crate::flex::layout_flex_children(
                style,
                element,
                content_x,
                content_start_y,
                content_width,
                explicit_content_height,
                font,
                style_cache,
                intrinsic,
                image_cache,
                reuse,
                depth + 1,
            )
        };
        children = flex_boxes;
        cursor_y = content_start_y + used_height;
    } else if style.display.is_grid() {
        // Milestone 33: grid layout (`grid.rs`).
        let explicit_content_height = definite_height(style).map(|h| match style.box_sizing {
            BoxSizing::BorderBox => (h - padding.top - padding.bottom - border_width.vertical()).max(0.0),
            BoxSizing::ContentBox => h,
        });
        let (grid_boxes, used_height) = if depth >= MAX_LAYOUT_DEPTH {
            (Vec::new(), 0.0)
        } else {
            crate::grid::layout_grid(style, element, content_x, content_start_y, content_width, explicit_content_height, (border_box_x, border_box_y), font, style_cache, intrinsic, image_cache, reuse, depth + 1)
        };
        children = grid_boxes;
        cursor_y = content_start_y + used_height;
    } else if style.display.is_table() {
        // Milestone 33: table layout (`table.rs`).
        let (table_boxes, used_height) = if depth >= MAX_LAYOUT_DEPTH {
            (Vec::new(), 0.0)
        } else {
            crate::table::layout_table(style, element, content_x, content_start_y, content_width, (border_box_x, border_box_y), font, style_cache, intrinsic, image_cache, reuse, depth + 1)
        };
        children = table_boxes;
        cursor_y = content_start_y + used_height;
    } else {
        (children, cursor_y) = layout_flow_children(
            element,
            style,
            (content_x, content_start_y, content_width),
            (border_box_x, border_box_y),
            exclusions_at_start,
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        );
    }

    // Everything below runs after the children are laid out: kept in a
    // function of its own, so its locals are not on the stack while this
    // one's descendants are laid out (the stack contract, `MIN_STACK_BYTES`).
    let measured = Measured {
        border_box: (border_box_x, border_box_y, border_box_width),
        content_width,
        content_start_y,
        cursor_y,
        padding,
        border_width,
        img_intrinsic,
        img_ratio,
        control_intrinsic,
        is_checkbox_or_radio,
        image,
        saved_exclusions,
        exclusions_at_start,
        forced_height,
        children,
    };
    finish_box(element, style, measured, (outer_x, outer_y, available_width), font, style_cache, intrinsic, image_cache, reuse, depth)
}

/// A block container's in-flow children — block boxes, lines of inline
/// content, floats — from `content` (`x`, `y`, width) down; returns them
/// and where the flow ended. Its own function so its locals are on the
/// stack only while a block container lays out its children, not under
/// every flex, grid or table level (the stack contract, `MIN_STACK_BYTES`).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn layout_flow_children(
    element: ElementRef<'_>,
    style: &Style,
    (content_x, content_start_y, content_width): (f32, f32, f32),
    (border_box_x, border_box_y): (f32, f32),
    exclusions_at_start: usize,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let mut cursor_y = content_start_y;
    let mut children;
    {
        children = Vec::new();
        let mut pending_inline: Vec<InlineWord> = Vec::new();

        // Milestone 16: `<input type="text">`/`<textarea>` are void
        // elements (html_parser gives them no children) whose visible
        // content is their `value` ATTRIBUTE, not any child node — so
        // it's synthesized as inline text here, reusing the exact same
        // word-wrapping/rendering machinery ordinary text content
        // already goes through. The visual caret indicator (Milestone
        // 16's own named gap) is now drawn — not here, though: it's a
        // paint-time overlay computed by `app` from these same word
        // boxes' real positions (see `app::caret_x_for_cursor`), not a
        // `layout`/cascade concept. Excludes `submit` (its `value` is
        // BUTTON label text, not "current typed content" — unaffected,
        // unchanged from Milestone 16) and `checkbox`/`radio` (their
        // `value` is what a real `<form>` would submit, never rendered
        // as visible text — Milestone 19 gives them a checked-state
        // fill instead, see below).
        // Milestone 33: a submit/reset/button input shows its `value` too —
        // it is the button's label.
        if (element.is(dom::atoms::INPUT) || element.is(dom::atoms::TEXTAREA))
            && !matches!(element.attr_atom(dom::atoms::TYPE), Some("checkbox") | Some("radio"))
        {
            let value = element.attr_atom(dom::atoms::VALUE).unwrap_or("");
            if !value.is_empty() {
                collect_words_from_text(value, style, None, &mut pending_inline);
            }
        }

        // `<img>` is a void element (html_parser already gives it no
        // children) — the loop below simply won't execute for one, which
        // is exactly right: an <img>'s "content" is its decoded image,
        // not a child box tree.
        //
        // Past `MAX_LAYOUT_DEPTH`, children are skipped entirely rather
        // than recursed into — see that constant's doc comment.
        if depth >= MAX_LAYOUT_DEPTH && element.has_children() {
            diagnostics::guard(diagnostics::Guard::LayoutDepth, "DOM deeper than layout walks; children skipped");
        }
        // Milestone 33: the bottom margin of the block box placed last, while
        // nothing (no line box) has come after it — adjoining vertical
        // margins of siblings collapse (CSS 2 §8.3.1).
        let mut previous_margin_bottom: Option<f32> = None;
        let generated = with_generated(element, style_cache);
        if depth < MAX_LAYOUT_DEPTH {
        for child in generated.iter() {
            match child {
                ChildRef::Text(text_content) => {
                    collect_words_from_text(text_content, style, None, &mut pending_inline);
                }
                // Milestone 33: `display` decides — no box, inline content
                // (text or atomic boxes on the line), or a block box.
                ChildRef::Element(child_el) if matches!(display_of(style_cache, child_el), Display::None | Display::TableColumn) => {}
                // Out of flow: only where it would have been is recorded;
                // its containing block lays it out (`positioned.rs`).
                ChildRef::Element(child_el) if is_out_of_flow(style_cache, child_el) => {
                    reuse.record_static_position(child_el.node_id(), element.node_id(), content_x - border_box_x, cursor_y - border_box_y);
                }
                // Milestone 33: a float goes to its side at the top of the
                // current line; the lines after flow around it.
                ChildRef::Element(child_el) if is_float(style_cache, child_el) => {
                    let placed = place_float(child_el, content_x, content_width, cursor_y, font, style_cache, intrinsic, image_cache, reuse, depth + 1);
                    children.push(placed);
                }
                ChildRef::Element(child_el) if child_el.is(dom::atoms::BR) || is_inline_level(display_of(style_cache, child_el)) => {
                    let child_style = style_of(style_cache, child_el);
                    let line_y = cursor_y;
                    let mut inline_floats = Vec::new();
                    push_inline_child(child_el, child_style, style_cache, &mut pending_inline, 0, &mut |el, st| {
                        if st.position.is_out_of_flow() {
                            reuse.record_static_position(el.node_id(), element.node_id(), content_x - border_box_x, line_y - border_box_y);
                            return None;
                        }
                        if st.float != style::Float::None {
                            inline_floats.push(place_float(el, content_x, content_width, line_y, font, style_cache, intrinsic, image_cache, reuse, depth + 1));
                            return None;
                        }
                        layout_atomic(el, st, content_width, font, style_cache, intrinsic, image_cache, reuse, depth + 1)
                    });
                    children.extend(inline_floats);
                }
                ChildRef::Element(child_el) => {
                    let before_lines = cursor_y;
                    flush_pending_inline(
                        &mut pending_inline,
                        &mut cursor_y,
                        &mut children,
                        content_x,
                        content_width,
                        style.text_align,
                        style.white_space,
                        font,
                        &reuse.exclusions,
                        style.text.rtl,
                    );
                    if cursor_y != before_lines {
                        previous_margin_bottom = None;
                    }
                    // `clear`: below the floats on the cleared sides.
                    let child_style_ref = style_cache.get(&child_el.node_id());
                    if let Some(clear_to) = child_style_ref.and_then(|s| crate::floats::clearance(&reuse.exclusions, s.clear)) {
                        if clear_to > cursor_y {
                            cursor_y = clear_to;
                            previous_margin_bottom = None;
                        }
                    }

                    // No longer a redundant second `resolve_style` call
                    // (see `layout_document`'s doc comment) — this is a
                    // second CACHE LOOKUP, which is O(1) and free of the
                    // inefficiency this line used to carry. `margin_of`
                    // (not `style_of(...).margin`) so this doesn't ALSO
                    // clone the child's whole `Style` a second time
                    // right before `layout_element` below clones it
                    // again internally via its own `style_of` call.
                    let child_margin = margin_of(style_cache, child_el, content_width);
                    // Place the child so that the gap between the previous
                    // sibling's border box and this one's is the collapsed
                    // margin, not the sum.
                    let child_outer_y = match previous_margin_bottom {
                        Some(bottom) => cursor_y - bottom + collapse_margins(bottom, child_margin.top) - child_margin.top,
                        None => cursor_y,
                    };

                    // A formatting-context root does not overlap floats: it
                    // is laid out in the room beside them.
                    let (child_x, child_width) = match child_style_ref {
                        Some(s) if !reuse.exclusions.is_empty() && establishes_formatting_context(s) => {
                            let (l, r) = crate::floats::line_interval(&reuse.exclusions, child_outer_y, 1.0, content_x, content_x + content_width);
                            (l, (r - l).max(0.0))
                        }
                        _ => (content_x, content_width),
                    };
                    let child_box = layout_element(
                        child_el,
                        child_x,
                        child_outer_y,
                        child_width,
                        font,
                        style_cache,
                        intrinsic,
                        image_cache,
                        reuse,
                        depth + 1,
                    );
                    // The flow continues from where the child is in flow,
                    // not where a relative offset drew it.
                    let child_dy = style_cache.get(&child_el.node_id()).map_or(0.0, |s| relative_offset(s, content_width).1);
                    cursor_y = child_box.y - child_dy + child_box.height + child_margin.bottom;
                    previous_margin_bottom = Some(child_margin.bottom);
                    if children.capacity() == 0 {
                        // Boxes are large: grow the list once, not by
                        // doubling (each growth copies every box so far).
                        children.reserve(element.child_count());
                    }
                    children.push(child_box);
                    reuse.link(child_el.node_id(), Some(element.node_id()), children.len() - 1);
                }
            }
        }
        }
        flush_pending_inline(
            &mut pending_inline,
            &mut cursor_y,
            &mut children,
            content_x,
            content_width,
            style.text_align,
            style.white_space,
            font,
            &reuse.exclusions,
            style.text.rtl,
        );
        // The container grows to hold the floats it placed (see
        // `floats.rs` for this simplification).
        if let Some(bottom) = reuse.exclusions[exclusions_at_start..].iter().map(|e| e.y1).reduce(f32::max) {
            cursor_y = cursor_y.max(bottom);
        }
    }
    (children, cursor_y)
}

/// What `layout_element_sized` measured before its children were laid out,
/// for [`finish_box`].
struct Measured {
    border_box: (f32, f32, f32),
    content_width: f32,
    content_start_y: f32,
    cursor_y: f32,
    padding: EdgeSizes,
    border_width: EdgeSizes,
    img_intrinsic: Option<(f32, f32, Arc<images::DecodedImage>)>,
    img_ratio: Option<f32>,
    control_intrinsic: Option<(f32, f32)>,
    is_checkbox_or_radio: bool,
    image: Option<Arc<images::DecodedImage>>,
    saved_exclusions: Option<Vec<crate::floats::Exclusion>>,
    exclusions_at_start: usize,
    forced_height: Option<f32>,
    children: Vec<LayoutBox>,
}

/// The box itself, once its children are laid out: its height, border,
/// paint properties, sticky and absolutely positioned descendants, and
/// relative offset.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn finish_box(
    element: ElementRef<'_>,
    style: &Style,
    m: Measured,
    (outer_x, outer_y, available_width): (f32, f32, f32),
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> LayoutBox {
    let Measured {
        border_box: (border_box_x, border_box_y, border_box_width),
        content_width,
        content_start_y,
        cursor_y,
        padding,
        border_width,
        img_intrinsic,
        img_ratio,
        control_intrinsic,
        is_checkbox_or_radio,
        image,
        saved_exclusions,
        exclusions_at_start,
        forced_height,
        mut children,
    } = m;
    let content_height = (cursor_y - content_start_y).max(0.0);
    let border_box_height = match forced_height.or_else(|| definite_height(style).map(|h| match style.box_sizing {
        BoxSizing::BorderBox => h,
        BoxSizing::ContentBox => h + padding.top + padding.bottom + border_width.vertical(),
    })) {
        Some(h) => h,
        None => match (&img_intrinsic, control_intrinsic) {
            (Some((_, h, _)), _) => match (style.width.is_some() || style.width_intrinsic.is_some(), img_ratio) {
                (true, Some(ratio)) => content_width * ratio + padding.top + padding.bottom + border_width.vertical(),
                _ => h + padding.top + padding.bottom + border_width.vertical(),
            },
            (None, Some((_, h))) => h + padding.top + padding.bottom + border_width.vertical(),
            (None, None) => content_height + padding.top + padding.bottom + border_width.vertical(),
        },
    };

    // `min-height` / `max-height` — pixel values; a percentage needs a
    // containing block of definite height, which block layout does not
    // track, so it does not constrain (CSS 2 §10.7).
    let vertical_extra = padding.top + padding.bottom + border_width.vertical();
    let border_box_height = clamp_to_min_max(
        border_box_height,
        style.limits().min_height.filter(|l| !l.has_percent()),
        style.limits().max_height.filter(|l| !l.has_percent()),
        0.0,
        style.box_sizing,
        vertical_extra,
    );

    // Milestone 19: a checkbox/radio gets a default VISIBLE border even
    // when the author set no CSS border — matches a real UA stylesheet
    // giving form controls a default appearance instead of being
    // invisible (an unchecked checkbox with no fill and no border
    // would otherwise render as nothing at all).
    let border = if style.has_border() {
        Some(Box::new(style.border().used()))
    } else if is_checkbox_or_radio {
        Some(Box::new(Border::uniform(1.5, BorderStyle::Solid, style::Color { r: 147, g: 153, b: 178, a: 255 })))
    } else {
        None
    };

    // Milestone 19: checked state renders as a solid fill — no glyph-
    // drawing primitive exists for a real checkmark, and a filled
    // square/circle is a legible, real indicator (this engine's bar
    // is "functional and legible," not pixel-perfect native styling —
    // the same standard Milestone 16's `<select>` scope note used).
    let is_checked = is_checkbox_or_radio && element.attr_atom(dom::atoms::CHECKED).is_some();
    let background_color =
        if is_checked { style::Color { r: 137, g: 180, b: 250, a: 255 } } else { style.background_color };
    // `radio` renders round (half its own size, so it's a perfect
    // circle regardless of author CSS); `checkbox` keeps whatever
    // `border-radius` CSS set (square by default).
    let border_radius = if is_checkbox_or_radio && element.attr_atom(dom::atoms::TYPE) == Some("radio") {
        border_box_width.min(border_box_height) / 2.0
    } else {
        style.border_radius
    };

    let outline = if style.outline_width > 0.0 && style.outline_style != BorderStyle::None {
        Some(Outline {
            width: style.outline_width,
            color: style.outline_color,
            style: style.outline_style,
            offset: style.outline_offset,
        })
    } else {
        None
    };

    // This container's floats stop affecting anything else; a formatting-
    // context root gets its ancestors' back.
    reuse.exclusions.truncate(exclusions_at_start);
    if let Some(saved) = saved_exclusions {
        reuse.exclusions = saved;
    }
    reuse.store(element.node_id(), outer_x, outer_y, available_width, depth, style.margin.resolve(available_width).bottom);

    // Milestone 33: a sticky child is kept within this element's content
    // box (its containing block).
    let (_, viewport_height) = reuse.viewport;
    for child in children.iter_mut().filter(|c| c.extras().positioning.scheme == style::Position::Sticky) {
        let Some(child_style) = child.element_key.and_then(|k| style_cache.get(&k)) else { continue };
        // This box's content box, at its final (possibly set) height —
        // relative to the sticky box (Milestone 39b), so it moves with it.
        let container_bottom = border_box_y + border_box_height - border_width.bottom - padding.bottom;
        let sticky = crate::Sticky {
            top: child_style.inset().top.and_then(style::Length::absolute),
            bottom: child_style.inset().bottom.and_then(style::Length::absolute),
            room_above: child.y - content_start_y,
            room_below: container_bottom - (child.y + child.height),
            viewport_height,
        };
        child.touch();
        child.extras_mut().positioning.sticky = Some(Box::new(sticky));
    }

    let mut laid = LayoutBox {
        x: border_box_x,
        y: border_box_y,
        width: border_box_width,
        height: border_box_height,
        background_color,
        border,
        border_radius,
        opacity: style.opacity,
        visible: style.visibility == style::Visibility::Visible,
        extras: {
            let extras = crate::BoxExtras {
                outline,
                image_is_content: img_intrinsic.is_some(),
                object_fit: style.object_fit,
                background_size: style.background_size,
                background_position: style.background_position,
                background_repeat: style.background_repeat,
                positioning: crate::Positioning { scheme: style.position, z_index: style.z_index, sticky: None },
                box_shadow: style.box_shadow.clone().into_boxed_slice(),
                transform: style.transform,
                promoted: style.composited_transform(),
            };
            (extras != crate::BoxExtras::default()).then(|| Box::new(extras))
        },
        // The root's and `<body>`'s `overflow` belong to the viewport (CSS
        // Overflow 3 §3.3), which the page scroll already is.
        overflow: if depth == 0 || element.is(dom::atoms::BODY) {
            crate::OverflowClip::default()
        } else {
            crate::OverflowClip {
                x: style.overflow_x.clips(),
                y: style.overflow_y.clips(),
                scrollable: style.overflow_x.user_scrollable() || style.overflow_y.user_scrollable(),
            }
        },
        image,
        element_key: Some(element.node_id()),
        text: None,
        children,
        subtree: Default::default(),
    };

    // A positioned element is the containing block for its absolute
    // descendants: its padding box, now that its size is known.
    if style.position.is_positioned() {
        let cb = crate::positioned::ContainingBlock {
            x: laid.x + border_width.left,
            y: laid.y + border_width.top,
            width: (laid.width - border_width.horizontal()).max(0.0),
            height: (laid.height - border_width.vertical()).max(0.0),
        };
        crate::positioned::place_descendants(
            element,
            &mut laid,
            cb,
            &[crate::positioned::Collect::Absolute],
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        );
    }
    // `position: relative` shifts the laid-out box (and everything in it)
    // by its insets; `left` wins over `right`, `top` over `bottom`. A
    // percentage `top`/`bottom` needs a definite containing height, which
    // is not tracked, so it counts as zero.
    let (dx, dy) = relative_offset(style, available_width);
    crate::reuse::translate(&mut laid, dx, dy);
    reuse.shift_descendants(element, dx, dy);
    laid
}

/// How far `position: relative` shifts a box (0 for any other position):
/// `left` wins over `right`, `top` over `bottom`. A percentage `top` /
/// `bottom` needs a definite containing height, which is not tracked, so
/// it counts as zero. The shift is visual only — the flow around the box
/// is laid out as if it were not there.
pub(crate) fn relative_offset(style: &Style, containing_width: f32) -> (f32, f32) {
    if style.position != style::Position::Relative {
        return (0.0, 0.0);
    }
    let dx = match (style.inset().left, style.inset().right) {
        (Some(l), _) => l.resolve(containing_width),
        (None, Some(r)) => -r.resolve(containing_width),
        (None, None) => 0.0,
    };
    let dy = match (style.inset().top, style.inset().bottom) {
        (Some(t), _) => t.absolute().unwrap_or(0.0),
        (None, Some(b)) => -b.absolute().unwrap_or(0.0),
        (None, None) => 0.0,
    };
    (dx, dy)
}

/// Whether a box establishes a new block formatting context (CSS 2 §9.4.1)
/// — floats outside it do not reach in, and it does not overlap them.
pub(crate) fn establishes_formatting_context(style: &Style) -> bool {
    style.overflow_x.clips()
        || style.overflow_y.clips()
        || style.display.is_flex_container()
        || style.display.is_atomic_inline()
        || style.display.is_table()
        || style.display.is_grid()
        || style.float != style::Float::None
        || style.position.is_out_of_flow()
}

/// Whether an element floats (and is in flow — an absolutely positioned
/// float is just absolutely positioned).
pub(crate) fn is_float(style_cache: &StyleCache, element: ElementRef<'_>) -> bool {
    style_cache.get(&element.node_id()).is_some_and(|s| s.float != style::Float::None && !s.position.is_out_of_flow())
}

/// Lays out a float (shrink-to-fit) and places it in its container's
/// content box at `y` or below (CSS 2 §9.5.1), below cleared floats, then
/// records it for the lines that follow.
#[allow(clippy::too_many_arguments)]
fn place_float(
    element: ElementRef<'_>,
    content_x: f32,
    content_width: f32,
    y: f32,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> LayoutBox {
    let style = style_of(style_cache, element);
    let margin = style.margin.resolve(content_width);
    let mut laid = layout_element_sized(element, 0.0, 0.0, content_width, None, None, font, style_cache, intrinsic, image_cache, reuse, depth);
    let (w, h) = (margin.left + laid.width + margin.right, margin.top + laid.height + margin.bottom);
    let y_min = crate::floats::clearance(&reuse.exclusions, style.clear).map_or(y, |c| c.max(y));
    let left = style.float != style::Float::Right;
    let (x, y) = crate::floats::place(&reuse.exclusions, content_x, content_x + content_width, y_min, w, h, left);
    let (dx, dy) = (x + margin.left - laid.x, y + margin.top - laid.y);
    crate::reuse::translate(&mut laid, dx, dy);
    if reuse.exclusions.len() < crate::floats::MAX_FLOATS {
        reuse.exclusions.push(crate::floats::Exclusion { x0: x, y0: y, x1: x + w, y1: y + h, left });
    } else {
        diagnostics::guard(diagnostics::Guard::LayoutDepth, "too many floats in one container; the rest do not exclude");
    }
    laid
}

/// Whether an element is taken out of normal flow.
pub(crate) fn is_out_of_flow(style_cache: &StyleCache, element: ElementRef<'_>) -> bool {
    style_cache.get(&element.node_id()).is_some_and(|s| s.position.is_out_of_flow())
}

#[cfg(test)]
mod tests {
    use crate::*;
    use css_parser::{parse_stylesheet, Stylesheet};
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



    // ---- Phase C: box model & paint ----

    fn box_for(style: &str) -> LayoutBox {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), style.into()));
        layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access())
    }



    // ---- Security regression: unbounded DOM nesting depth ----

    /// Without `MAX_LAYOUT_DEPTH` (and the matching caps in
    /// `tree_needs_intrinsic_sizing`/`measure_node`/
    /// `collect_needed_image_paths`, which all run before/alongside
    /// `layout_element`), a sufficiently deep tree overflows the native
    /// stack during layout — independent of `html_parser`'s own
    /// `MAX_NESTING_DEPTH`, which only bounds HTML-PARSED trees, not
    /// one built directly (the same shape a JS `appendChild` loop could
    /// produce). This must complete without crashing.
    #[test]
    fn laying_out_an_extremely_deep_tree_does_not_overflow_the_stack() {
        let depth = 20_000;
        let mut node = Element::new("div");
        node.attributes.push(("style".into(), "width:min-content".into())); // forces the intrinsic-sizing pass too
        for _ in 0..depth {
            let mut parent = Element::new("div");
            parent.children.push(Node::Element(node));
            node = parent;
        }
        let laid_out = layout_document(&node, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        // The real assertion is just that this returned instead of
        // aborting; a rough sanity check that SOMETHING sensible came
        // back.
        assert!(laid_out.width > 0.0);
    }


    #[test]
    fn explicit_size_is_respected() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:100px;height:50px".into()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.width, 100.0);
        assert_eq!(laid_out.height, 50.0);
    }


    #[test]
    fn children_stack_vertically() {
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "width:200px".into()));

        let mut child_a = Element::new("div");
        child_a.attributes.push(("style".into(), "height:30px".into()));
        let mut child_b = Element::new("div");
        child_b.attributes.push(("style".into(), "height:40px".into()));

        parent.children.push(Node::Element(child_a));
        parent.children.push(Node::Element(child_b));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 2);
        assert_eq!(laid_out.children[0].y, 0.0);
        assert_eq!(laid_out.children[1].y, 30.0);
        assert_eq!(laid_out.height, 70.0);
    }


    #[test]
    fn width_defaults_to_available_width() {
        let el = Element::new("div");
        let laid_out = layout_document(&el, 640.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.width, 640.0);
    }


    #[test]
    fn text_and_element_children_stack_together() {
        let mut parent = Element::new("div");
        parent.children.push(Node::Text("Some text".to_string()));
        let mut child = Element::new("div");
        child.attributes.push(("style".into(), "height:20px".into()));
        parent.children.push(Node::Element(child));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 3);
    }


    #[test]
    fn custom_font_size_changes_measured_size() {
        let font = test_font();
        let mut small = Element::new("div");
        small.children.push(Node::Text("Test".to_string()));
        let small_box = layout_document(&small, 800.0, &font, &no_sheet(), &mut ImageCache::with_local_file_access());

        let mut big = Element::new("div");
        big.children.push(Node::Text("Test".to_string()));
        big.attributes.push(("style".into(), "font-size:48px".into()));
        let big_box = layout_document(&big, 800.0, &font, &no_sheet(), &mut ImageCache::with_local_file_access());

        assert!(big_box.children[0].width > small_box.children[0].width);
    }


    #[test]
    fn stylesheet_class_selector_affects_layout() {
        let sheet = parse_stylesheet(".big { width: 300px; }");
        let mut el = Element::new("div");
        el.attributes.push(("class".into(), "big".into()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &sheet, &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.width, 300.0);
    }


    #[test]
    fn descendant_selector_matches_through_layout_recursion() {
        let sheet = parse_stylesheet("div span { color: #ff0000; }");
        let mut parent = Element::new("div");
        let mut span = Element::new("span");
        span.children.push(Node::Text("colored text".to_string()));
        parent.children.push(Node::Element(span));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &sheet, &mut ImageCache::with_local_file_access());
        let text = laid_out.children[0].text.as_ref().unwrap();
        assert_eq!(text.color, style::Color { r: 255, g: 0, b: 0, a: 255 });
    }


    #[test]
    fn color_inherits_down_through_layout_recursion() {
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "color:#00ff00".into()));
        let mut child = Element::new("div");
        child.children.push(Node::Text("inherited color".to_string()));
        parent.children.push(Node::Element(child));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        let child_box = &laid_out.children[0];
        let text = child_box.children[0].text.as_ref().unwrap();
        assert_eq!(text.color, style::Color { r: 0, g: 255, b: 0, a: 255 });
    }


    #[test]
    fn padding_insets_content_from_border_box_edge() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:200px;padding:20px".into()));
        el.children.push(Node::Text("word".to_string()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children[0].x, 20.0);
    }


    #[test]
    fn border_adds_frame_and_is_none_without_border_style() {
        let mut with_style = Element::new("div");
        with_style.attributes.push(("style".into(), "border:3px solid #ff0000".into()));
        let box_with = layout_document(&with_style, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(box_with.border, Some(Box::new(Border::uniform(3.0, style::BorderStyle::Solid, style::Color { r: 255, g: 0, b: 0, a: 255 }))));
    }


    /// M6's gap: each side's width shapes the box — the content starts
    /// after the top and left widths and the box grows by all four.
    #[test]
    fn each_border_side_adds_its_own_width() {
        let mut root = Element::new("div");
        root.attributes.push(("style".into(), "width: 100px; border-top: 3px solid red; border-left: 7px solid blue; border-bottom: 1px solid".into()));
        let mut child = Element::new("div");
        child.attributes.push(("style".into(), "height: 10px".into()));
        root.append_child(child);
        let laid = layout_document(&root, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!((laid.width, laid.height), (107.0, 14.0), "100 + 7 wide; 3 + 10 + 1 tall");
        let border = laid.border.as_deref().copied().expect("a border");
        assert_eq!((border.top.width, border.right.width, border.bottom.width, border.left.width), (3.0, 0.0, 1.0, 7.0));
        let inner = &laid.children[0];
        assert_eq!((inner.x, inner.y), (laid.x + 7.0, laid.y + 3.0));
        assert_eq!(laid.padding_box(), (laid.x + 7.0, laid.y + 3.0, 100.0, 10.0));
    }

    #[test]
    fn content_box_vs_border_box_produce_different_sizes_for_same_inputs() {
        let base_style = "width:100px;padding:10px;border:5px solid #000000";
        let mut content_box_el = Element::new("div");
        content_box_el.attributes.push(("style".into(), base_style.to_string()));
        let content_box = layout_document(&content_box_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());

        let mut border_box_el = Element::new("div");
        border_box_el.attributes.push(("style".into(), format!("{base_style};box-sizing:border-box")));
        let border_box = layout_document(&border_box_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());

        assert_eq!(content_box.width, 130.0);
        assert_eq!(border_box.width, 100.0);
    }


    #[test]
    fn margin_creates_space_around_border_box_not_inside_it() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:100px;margin:20px".into()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.x, 20.0);
        assert_eq!(laid_out.width, 100.0);
    }


    // ---- Milestone 7: images ----

    #[test]
    fn img_element_sizes_to_its_intrinsic_dimensions() {
        let mut img = Element::new("img");
        img.attributes.push(("src".into(), fixture_path("quadrants.png")));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        // quadrants.png is 8x4.
        assert_eq!(laid_out.width, 8.0);
        assert_eq!(laid_out.height, 4.0);
        assert!(laid_out.image.is_some());
    }


    #[test]
    fn img_explicit_width_overrides_intrinsic_size() {
        let mut img = Element::new("img");
        img.attributes.push(("src".into(), fixture_path("quadrants.png")));
        img.attributes.push(("style".into(), "width:100px;height:50px".into()));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert_eq!(laid_out.width, 100.0);
        assert_eq!(laid_out.height, 50.0);
    }


    #[test]
    fn img_with_missing_src_degrades_gracefully_not_a_panic() {
        let img = Element::new("img"); // no src attribute at all
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert!(laid_out.image.is_none());
    }


    #[test]
    fn img_with_bad_path_degrades_gracefully_not_a_panic() {
        let mut img = Element::new("img");
        img.attributes.push(("src".into(), "/nonexistent/path.png".into()));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert!(laid_out.image.is_none());
        // An image that failed to load, with no size given, has no size
        // (Milestone 33: `<img>` is an inline replaced element, not a
        // full-width block) — rather than crashing.
        assert_eq!((laid_out.width, laid_out.height), (0.0, 0.0));
    }


    #[test]
    fn background_image_attaches_without_affecting_box_size() {
        let mut el = Element::new("div");
        el.attributes.push((
            "style".into(),
            format!("width:200px;height:100px;background-image:url({})", fixture_path("solid.jpg")),
        ));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut cache);
        // Explicit width/height are respected — background-image never
        // resizes the box (unlike <img>'s intrinsic-size behavior).
        assert_eq!(laid_out.width, 200.0);
        assert_eq!(laid_out.height, 100.0);
        assert!(laid_out.image.is_some());
    }


    #[test]
    fn img_is_treated_as_a_leaf_even_though_html_parser_wont_give_it_children() {
        // Defensive: even if an <img> somehow had children (shouldn't
        // happen — html_parser treats it as a void element), layout
        // must not try to lay them out as normal content, since an
        // <img>'s content IS the decoded image.
        let mut img = Element::new("img");
        img.attributes.push(("src".into(), fixture_path("quadrants.png")));
        let mut cache = ImageCache::with_local_file_access();
        let laid_out = layout_document(&img, 800.0, &test_font(), &no_sheet(), &mut cache);
        assert!(laid_out.children.is_empty());
    }


    // ---- Milestone 14: transform ----

    #[test]
    fn transform_style_flows_through_to_layout_box() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "transform:translate(10px,20px) scale(2)".into()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.extras().transform.translate_x, 10.0);
        assert_eq!(laid_out.extras().transform.translate_y, 20.0);
        assert_eq!(laid_out.extras().transform.scale_x, 2.0);
    }


    #[test]
    fn no_transform_style_yields_identity_transform() {
        let el = Element::new("div");
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.extras().transform, style::Transform::default());
    }


    // ---- Regression: <script>/<style> source text must never render ----

    #[test]
    fn script_tag_source_text_is_never_laid_out_as_content() {
        // Reproduces the exact bug seen on real hardware: a <script>
        // element fell through to the ordinary block-element path
        // (it's not in INLINE_TAGS), so its own text child — the raw
        // JS source — got laid out and rendered like any other text
        // content. A <script> must produce NO layout box and NO word
        // boxes at all, regardless of where in the tree it appears.
        let mut parent = Element::new("div");
        parent.children.push(Node::Text("before".to_string()));
        let mut script = Element::new("script");
        script.children.push(Node::Text("document.getElementById('x').textContent = 'y';".to_string()));
        parent.children.push(Node::Element(script));
        parent.children.push(Node::Text("after".to_string()));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        // Only "before" and "after" should have produced word boxes —
        // nothing from the script's source text.
        assert_eq!(laid_out.children.len(), 2);
        assert_eq!(laid_out.children[0].text.as_ref().unwrap().content, "before");
        assert_eq!(laid_out.children[1].text.as_ref().unwrap().content, "after");
    }


    #[test]
    fn style_tag_source_text_is_never_laid_out_as_content() {
        // Same bug, for <style> — matters for pages where <style>
        // appears inside <body> (unusual but valid HTML), not just
        // inside <head> (which app's current demo already avoids by
        // only laying out <body> — this test covers the case where
        // that avoidance doesn't apply).
        let mut parent = Element::new("div");
        let mut style_el = Element::new("style");
        style_el.children.push(Node::Text("div { color: red; }".to_string()));
        parent.children.push(Node::Element(style_el));
        parent.children.push(Node::Text("visible".to_string()));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 1);
        assert_eq!(laid_out.children[0].text.as_ref().unwrap().content, "visible");
    }


    #[test]
    fn script_tag_is_invisible_even_as_the_only_child() {
        let mut parent = Element::new("div");
        let mut script = Element::new("script");
        script.children.push(Node::Text("console.log('hi');".to_string()));
        parent.children.push(Node::Element(script));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 0);
    }


    #[test]
    fn script_nested_inside_inline_content_is_also_skipped() {
        // Covers the `collect_inline_words` path (a <script> nested
        // inside an inline element like <span>), not just the main
        // block/inline dispatch loop — the same class of bug could
        // exist there too if only one of the two call sites were fixed.
        let mut parent = Element::new("div");
        parent.children.push(Node::Text("start ".to_string()));
        let mut span = Element::new("span");
        let mut script = Element::new("script");
        script.children.push(Node::Text("var x = 1;".to_string()));
        span.children.push(Node::Element(script));
        span.children.push(Node::Text("mid".to_string()));
        parent.children.push(Node::Element(span));
        parent.children.push(Node::Text(" end".to_string()));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        let words: Vec<&str> = laid_out.children.iter().map(|b| b.text.as_ref().unwrap().content.as_str()).collect();
        assert_eq!(words, vec!["start", "mid", "end"]);
    }


    // ---- Milestone 16: input value rendering ----

    #[test]
    fn input_value_renders_as_visible_text() {
        let mut input = Element::new("input");
        input.attributes.push(("value".into(), "hello".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 1);
        assert_eq!(laid_out.children[0].text.as_ref().unwrap().content, "hello");
    }


    #[test]
    fn empty_input_value_produces_no_text_boxes() {
        let input = Element::new("input");
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 0);
    }


    #[test]
    fn textarea_value_also_renders_as_text() {
        let mut textarea = Element::new("textarea");
        textarea.attributes.push(("value".into(), "some notes".into()));
        let laid_out = layout_document(&textarea, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        let words: Vec<&str> = laid_out.children.iter().map(|b| b.text.as_ref().unwrap().content.as_str()).collect();
        assert_eq!(words, vec!["some", "notes"]);
    }


    #[test]
    fn submit_input_shows_its_value_as_its_label_and_fits_it() {
        // <input type="submit" value="Go"> uses `value` as its button
        // label. Milestone 33 draws it (it used to be left out, which
        // rendered every submit button blank), and the button shrinks to
        // fit it instead of filling the line. Whether a control is
        // *editable* is decided by `dom::Element::is_text_editable`, not
        // by layout.
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "submit".into()));
        input.attributes.push(("value".into(), "Go".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 1);
        assert_eq!(laid_out.children[0].text.as_ref().map(|t| t.content.as_str()), Some("Go"));
        assert!(laid_out.width < 100.0, "shrink-to-fit, not the container's 800px: {}", laid_out.width);
        assert!(!input.is_text_editable());
    }


    // ---- Milestone 19: checkbox/radio ----

    #[test]
    fn checkbox_defaults_to_a_small_fixed_size_not_the_full_container_width() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "checkbox".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.width, 16.0);
        assert_eq!(laid_out.height, 16.0);
    }


    #[test]
    fn checkbox_does_not_render_its_value_as_text() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "checkbox".into()));
        input.attributes.push(("value".into(), "yes".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.children.len(), 0, "a checkbox's value must never render as visible text");
    }


    #[test]
    fn unchecked_checkbox_gets_a_default_visible_border_with_no_css() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "checkbox".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert!(laid_out.border.is_some(), "an unchecked checkbox must still be visible without any CSS");
    }


    #[test]
    fn checked_checkbox_renders_a_filled_background() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "checkbox".into()));
        input.attributes.push(("checked".into(), "checked".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.background_color, style::Color { r: 137, g: 180, b: 250, a: 255 });
    }


    #[test]
    fn radio_renders_fully_round_via_border_radius() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "radio".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.border_radius, laid_out.width.min(laid_out.height) / 2.0);
    }


    #[test]
    fn explicit_css_size_overrides_the_checkbox_default() {
        let mut input = Element::new("input");
        input.attributes.push(("type".into(), "checkbox".into()));
        input.attributes.push(("style".into(), "width:30px;height:30px".into()));
        let laid_out = layout_document(&input, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert_eq!(laid_out.width, 30.0);
        assert_eq!(laid_out.height, 30.0);
    }


    #[test]
    fn dashed_border_is_carried_on_the_layout_box() {
        let b = box_for("width:100px;height:40px;border:2px dashed #ff0000");
        let border = b.border.expect("dashed border should still produce a Border");
        assert_eq!(border.top.style, BorderStyle::Dashed);
        assert_eq!(border.top.width, 2.0);
        // Dashed still reserves border-box space like solid.
        assert_eq!(b.width, 104.0);
    }


    #[test]
    fn border_style_none_still_means_no_border() {
        let b = box_for("width:100px;border-width:5px;border-color:#000000");
        assert!(b.border.is_none());
        assert_eq!(b.width, 100.0);
    }


    #[test]
    fn outline_is_captured_and_does_not_affect_box_size() {
        let b = box_for("width:100px;height:40px;outline:3px solid #ff0000;outline-offset:2px");
        assert_eq!(b.width, 100.0); // outline never changes layout
        let o = b.extras().outline.expect("outline should be present");
        assert_eq!(o.width, 3.0);
        assert_eq!(o.offset, 2.0);
        assert_eq!(o.style, BorderStyle::Solid);
    }


    #[test]
    fn border_radius_is_carried_on_the_layout_box() {
        let b = box_for("width:100px;height:40px;border-radius:12px;background-color:#ff0000");
        assert_eq!(b.border_radius, 12.0);
    }

    // ---- Milestone 27: the layout depth cap is observable ----

    #[test]
    fn a_tree_deeper_than_the_layout_cap_bumps_the_guard_counter() {
        // Style/layout pre-passes run on `rayon` workers, so assert on the
        // process-global counter (deltas only — parallel tests share it).
        let before = diagnostics::metrics::snapshot();
        let mut node = Element::new("div");
        for _ in 0..(crate::MAX_LAYOUT_DEPTH + 50) {
            let mut parent = Element::new("div");
            parent.children.push(Node::Element(node));
            node = parent;
        }
        let _ = layout_document(&node, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.layout_depth") >= 1);
    }

    // ---- Milestone 33: length-percentages and `auto` margins ----

    fn child_of(parent_style: &str, child_style: &str) -> LayoutBox {
        let mut parent = Element::new("div");
        parent.set_attr("style", parent_style);
        let mut child = Element::new("div");
        child.set_attr("style", child_style);
        parent.append_child(child);
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        laid.children[0].clone()
    }

    #[test]
    fn percentages_resolve_against_the_containing_block_width() {
        let b = child_of("width: 400px", "width: 50%; height: 10px");
        assert_eq!(b.width, 200.0);
        let b = child_of("width: 400px", "width: calc(100% - 40px); height: 10px");
        assert_eq!(b.width, 360.0);
        // Padding and margin percentages are of the WIDTH, vertical ones too.
        let b = child_of("width: 400px", "margin: 5% 0 0 10%; padding-top: 25%; height: 0");
        assert_eq!((b.x, b.y, b.height), (40.0, 20.0, 100.0));
    }

    #[test]
    fn auto_horizontal_margins_centre_a_box_with_a_definite_width() {
        let b = child_of("width: 400px", "width: 100px; margin: 0 auto; height: 1px");
        assert_eq!(b.x, 150.0);
        let b = child_of("width: 400px", "width: 100px; margin-left: auto; height: 1px");
        assert_eq!(b.x, 300.0, "one auto margin takes all the free space");
        let b = child_of("width: 400px", "margin: 0 auto; height: 1px");
        assert_eq!((b.x, b.width), (0.0, 400.0), "an auto-width box fills the line; its auto margins are zero");
        let b = child_of("width: 400px", "width: 500px; margin: 0 auto; height: 1px");
        assert_eq!(b.x, 0.0, "an over-wide box is not pushed left of its container");
    }

    #[test]
    fn a_percentage_height_without_a_definite_containing_height_is_auto() {
        let b = child_of("width: 400px", "height: 50%");
        assert_eq!(b.height, 0.0);
    }

    #[test]
    fn font_relative_units_size_boxes() {
        let b = child_of("font-size: 20px; width: 800px", "width: 10em; font-size: 2em; height: 1rem");
        assert_eq!(b.width, 400.0, "em is the element's own (computed) font size");
        assert_eq!(b.height, 20.0, "rem is the root element's font size (the parent here), not the child's");
    }

    #[test]
    fn adjoining_sibling_margins_collapse() {
        let mut parent = Element::new("div");
        for (top, bottom) in [("0", "20px"), ("30px", "10px"), ("-5px", "0"), ("-10px", "0")] {
            let mut child = Element::new("div");
            child.set_attr("style", format!("height: 10px; margin-top: {top}; margin-bottom: {bottom}"));
            parent.append_child(child);
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        let ys: Vec<f32> = laid.children.iter().map(|c| c.y).collect();
        // 20 & 30 → 30; 10 & -5 → 5; 0 & -10 → -10.
        assert_eq!(ys, vec![0.0, 40.0, 55.0, 55.0]);
        assert_eq!(super::collapse_margins(-3.0, -7.0), -7.0);
    }

    #[test]
    fn a_line_box_between_blocks_stops_their_margins_collapsing() {
        let mut parent = Element::new("div");
        let mut a = Element::new("div");
        a.set_attr("style", "height: 10px; margin-bottom: 20px");
        let mut b = Element::new("div");
        b.set_attr("style", "height: 10px; margin-top: 20px");
        parent.append_child(a);
        parent.children.push(Node::Text("words".into()));
        parent.append_child(b);
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::with_local_file_access());
        let last = laid.children.last().unwrap();
        assert!(last.y > 10.0 + 20.0 + 20.0, "both margins apply around the line: y = {}", last.y);
    }

    // ---- Milestone 33: `display` ----

    fn laid(html_body: &str, css: &str) -> LayoutBox {
        let root = html_parser::parse_document(&format!("<html><body>{html_body}</body></html>")).unwrap();
        let body = root.children.iter().find_map(|n| match n { Node::Element(e) if e.tag == "body" => Some(e.clone()), _ => None }).unwrap();
        layout_document(&body, 800.0, &test_font(), &parse_stylesheet(css), &mut ImageCache::with_local_file_access())
    }

    fn texts(b: &LayoutBox, out: &mut Vec<String>) {
        if let Some(t) = &b.text {
            out.push(t.content.clone());
        }
        for c in &b.children {
            texts(c, out);
        }
    }

    #[test]
    fn display_none_removes_the_element_and_its_subtree() {
        let b = laid("<div class=a>gone <span>too</span></div><div>kept</div><span class=a>x</span>", ".a { display: none }");
        let mut t = Vec::new();
        texts(&b, &mut t);
        assert_eq!(t, vec!["kept"]);
        let b = laid("<p hidden>h</p><script>var x;</script><p>v</p>", "");
        let mut t = Vec::new();
        texts(&b, &mut t);
        assert_eq!(t, vec!["v"], "[hidden] and <script> are display: none");
        let b = laid("<p hidden>shown</p>", "p { display: block }");
        let mut t = Vec::new();
        texts(&b, &mut t);
        assert_eq!(t, vec!["shown"], "author CSS overrides the hidden attribute's default");
    }

    #[test]
    fn display_inline_and_block_override_the_tag_default() {
        // Two divs made inline share a line; a span made block gets its own.
        let b = laid("<div>one</div><div>two</div>", "div { display: inline }");
        assert_eq!(b.children.len(), 2);
        assert_eq!(b.children[0].y, b.children[1].y, "same line");
        let b = laid("a<span>b</span>c", "span { display: block }");
        assert_eq!(b.children.len(), 3);
        assert!(b.children[1].y > b.children[0].y && b.children[2].y > b.children[1].y);
    }

    #[test]
    fn inline_blocks_sit_on_the_line_and_shrink_to_fit() {
        let b = laid("<div class=ib>alpha</div> <div class=ib>beta</div> text", ".ib { display: inline-block; padding: 5px; border: 1px solid }");
        let (first, second) = (&b.children[0], &b.children[1]);
        assert_eq!(first.y, second.y, "same line");
        assert!(second.x > first.x + first.width, "side by side");
        assert!(first.width < 100.0, "shrink-to-fit: {}", first.width);
        assert_eq!(first.children[0].text.as_ref().unwrap().content, "alpha");
        // A definite width is honoured, and auto margins do not centre it.
        let b = laid("<div class=ib>x</div>", ".ib { display: inline-block; width: 200px; margin: 0 auto }");
        assert_eq!((b.children[0].x, b.children[0].width), (0.0, 200.0));
        // Baseline alignment: the text inside lines up with the text outside.
        let b = laid("out<div class=ib>in</div>", ".ib { display: inline-block; padding-top: 20px }");
        let out_baseline = b.children[0].text.as_ref().unwrap().baseline_y;
        let in_baseline = b.children[1].children[0].text.as_ref().unwrap().baseline_y;
        assert_eq!(out_baseline, in_baseline);
    }

    #[test]
    fn inline_blocks_wrap_as_units() {
        let b = laid(&"<div class=ib>word word</div> ".repeat(20), ".ib { display: inline-block; width: 150px }");
        let rows: std::collections::BTreeSet<i64> = b.children.iter().map(|c| c.y as i64).collect();
        assert!(rows.len() >= 4, "150px boxes wrap across an 800px line: {rows:?}");
        assert!(b.children.iter().all(|c| c.x + c.width <= 800.0 + 0.5));
    }

    #[test]
    fn inline_flex_is_an_atomic_flex_container() {
        let b = laid("<nav><a>one</a><a>two</a></nav> after", "nav { display: inline-flex; gap: 10px }");
        let nav = &b.children[0];
        assert_eq!(nav.children.len(), 2, "flex items");
        assert!(nav.children[1].x >= nav.children[0].x + nav.children[0].width + 10.0 - 0.01);
        assert_eq!(b.children[1].y, nav.y, "the text after it is on the same line");
    }

    #[test]
    fn presentational_size_attributes_and_aspect_ratio() {
        let b = laid(r#"<svg width="24" height="12"></svg><canvas></canvas><img width="50%">"#, "");
        assert_eq!((b.children[0].width, b.children[0].height), (24.0, 12.0));
        assert_eq!((b.children[1].width, b.children[1].height), (300.0, 150.0), "the default object size");
        let b = laid(r#"<svg width="24" height="12"></svg>"#, "svg { width: 48px }");
        assert_eq!(b.children[0].width, 48.0, "CSS beats the attribute");
        let img = format!(r#"<img src="{}" style="width: 20px">"#, fixture_path("quadrants.png"));
        let b = laid(&img, "");
        let (w, h) = (b.children[0].width, b.children[0].height);
        let decoded = ImageCache::with_local_file_access().load_from_file(&fixture_path("quadrants.png")).unwrap();
        assert_eq!(w, 20.0);
        assert!((h - 20.0 * decoded.height as f32 / decoded.width as f32).abs() < 0.01, "aspect ratio kept: {w}x{h}");
    }

    #[test]
    fn form_controls_have_default_sizes() {
        let b = laid(r#"<input type=text><textarea></textarea><input type=hidden><button>Go</button>"#, "");
        assert_eq!(b.children[0].width, 150.0);
        assert_eq!(b.children[1].width, 180.0);
        assert_eq!(b.children.len(), 3, "type=hidden is not rendered");
        assert!(b.children[2].width < 60.0, "a button fits its label");
    }

    #[test]
    fn min_and_max_width_and_height_clamp() {
        let b = child_of("width: 400px", "max-width: 100px; height: 1px");
        assert_eq!(b.width, 100.0);
        let b = child_of("width: 400px", "width: 50px; min-width: 25%; height: 1px");
        assert_eq!(b.width, 100.0);
        let b = child_of("width: 400px", "max-width: 50px; min-width: 80px; height: 1px");
        assert_eq!(b.width, 80.0, "min wins over max");
        let b = child_of("width: 400px", "max-width: 100px; padding: 0 10px; box-sizing: border-box; height: 1px");
        assert_eq!(b.width, 100.0);
        let b = child_of("width: 400px", "max-width: 100px; padding: 0 10px; height: 1px");
        assert_eq!(b.width, 120.0, "content-box: the max is the content width");
        let b = child_of("width: 400px", "min-height: 30px");
        assert_eq!(b.height, 30.0);
        let b = child_of("width: 400px", "height: 90px; max-height: 40px");
        assert_eq!(b.height, 40.0);
    }

    #[test]
    fn max_width_with_auto_margins_centres_the_box() {
        let b = child_of("width: 400px", "max-width: 200px; margin: 0 auto; height: 1px");
        assert_eq!((b.x, b.width), (100.0, 200.0));
    }

    // ---- Milestone 33: generated content ----

    #[test]
    fn before_and_after_are_laid_out_around_the_content_and_hit_as_their_element() {
        let b = laid("<p id=p>middle</p>", "p::before { content: 'first ' } p::after { content: ' last' }");
        let p = &b.children[0];
        let mut t = Vec::new();
        texts(p, &mut t);
        assert_eq!(t, vec!["first", "middle", "last"]);
        // A click on the generated text is a click on the paragraph.
        let first = &p.children[0];
        let hit = hit_test_path(&b, first.x + 1.0, first.y + 1.0);
        assert_eq!(hit.first().copied(), p.element_key);
        assert!(hit.iter().all(|k| !style::is_pseudo(*k)));
    }

    #[test]
    fn a_block_or_empty_generated_box_takes_its_size() {
        let b = laid("<div>x</div>", "div::after { content: ''; display: block; height: 7px; background-color: red }");
        let div = &b.children[0];
        let after = div.children.last().unwrap();
        assert_eq!(after.height, 7.0);
        assert_eq!(div.height, after.y + 7.0 - div.y);
    }

    // ---- Milestone 33: floats ----

    #[test]
    fn text_flows_around_a_float_then_returns_to_full_width() {
        let words = "word ".repeat(60);
        let b = laid(&format!("<div id=c><div id=f></div>{words}</div>"), "#c { width: 300px } #f { float: left; width: 100px; height: 40px; margin-right: 10px }");
        let c = &b.children[0];
        let float = &c.children[0];
        assert_eq!((float.x, float.y, float.width), (c.x, c.y, 100.0));
        let texts: Vec<&LayoutBox> = c.children.iter().filter(|w| w.text.is_some()).collect();
        let beside: Vec<_> = texts.iter().filter(|w| w.y < float.y + 40.0).collect();
        let below: Vec<_> = texts.iter().filter(|w| w.y >= float.y + 40.0).collect();
        assert!(!beside.is_empty() && beside.iter().all(|w| w.x >= c.x + 110.0 - 0.01), "lines beside the float start after it");
        assert!(below.iter().any(|w| (w.x - c.x).abs() < 0.01), "below it, lines use the full width again");
    }

    #[test]
    fn floats_stack_sideways_and_clear_moves_below_them() {
        let b = laid("<div id=c><div class=f>a</div><div class=f>b</div><div class=r>c</div><div id=after>x</div></div>",
            "#c { width: 300px } .f { float: left; width: 100px; height: 20px } .r { float: right; width: 50px; height: 30px } #after { clear: both }");
        let c = &b.children[0];
        let xs: Vec<(f32, f32)> = c.children[..3].iter().map(|f| (f.x - c.x, f.y - c.y)).collect();
        assert_eq!(xs, vec![(0.0, 0.0), (100.0, 0.0), (250.0, 0.0)]);
        let after = c.children.iter().rev().find(|b| b.text.is_none()).unwrap();
        assert_eq!(after.y, c.y + 30.0, "cleared below the tallest float");
    }

    #[test]
    fn a_container_holds_its_floats_and_a_formatting_root_sits_beside_them() {
        let b = laid("<div id=c><div id=f>x</div><div id=bfc>y</div></div>", "#c { width: 300px } #f { float: left; width: 120px; height: 50px } #bfc { overflow: hidden; height: 10px }");
        let c = &b.children[0];
        assert_eq!(c.height, 50.0, "grows to contain the float");
        let bfc = &c.children[1];
        assert_eq!((bfc.x - c.x, bfc.width), (120.0, 180.0), "beside the float, narrowed");
    }

    #[test]
    fn a_float_inside_inline_content_is_placed_too() {
        let b = laid("<p><a href=x><img style='float: right; width: 40px; height: 40px'> link</a> text</p>", "p { width: 200px; margin: 0 }");
        let p = &b.children[0];
        let img = p.children.iter().find(|c| c.text.is_none() && c.width == 40.0).expect("the float has a box");
        assert_eq!(img.x, p.x + 160.0);
    }
}
