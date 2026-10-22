//! Milestone 33: out-of-flow boxes — `position: absolute` and `fixed`
//! (CSS 2 §9.3.1, §10.3.7, §10.6.4).
//!
//! Normal-flow layout skips an out-of-flow element, recording only its
//! *static position* (where it would have been). When its containing block
//! has been laid out — the nearest positioned ancestor, or the viewport
//! (the initial containing block) — the element is laid out against that
//! block's padding box and its box is hoisted into the containing block's
//! box, where paint order (`crate::paint_order`) and hit testing find it.
//!
//! An out-of-flow box is never reused on its own (it is not linked into
//! `LayoutReuse`'s tree); it moves with its containing block's box when
//! that is reused.

use crate::box_model::{layout_element_sized, style_of};
use crate::intrinsic::IntrinsicCache;
use crate::reuse::translate;
use crate::{LayoutBox, MAX_LAYOUT_DEPTH};
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use style::{Display, Position, StyleCache};
use text::Font;

/// A containing block's padding box, in layout coordinates.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContainingBlock {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Which out-of-flow descendants a walk collects.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Collect {
    /// `absolute` descendants whose nearest positioned ancestor is the
    /// walk's start (the walk does not enter positioned descendants).
    Absolute,
    /// Every `fixed` element (the viewport contains them all).
    Fixed,
}

/// Collects `start`'s out-of-flow descendants of the given kind, in tree
/// order. Iterative (trees can be as deep as the layout depth cap);
/// `display: none` subtrees generate no boxes and are skipped.
pub(crate) fn collect<'a>(start: ElementRef<'a>, style_cache: &StyleCache, kind: Collect) -> Vec<ElementRef<'a>> {
    let mut found = Vec::new();
    let mut stack: Vec<(ElementRef<'_>, usize)> = element_children(start).rev().map(|c| (c, 1)).collect();
    while let Some((el, depth)) = stack.pop() {
        let Some(style) = style_cache.get(&el.node_id()) else { continue };
        if style.display == Display::None || depth >= MAX_LAYOUT_DEPTH {
            continue;
        }
        let descend = match (kind, style.position) {
            (Collect::Absolute, Position::Absolute) => {
                found.push(el);
                false // its own absolute descendants are its business
            }
            (Collect::Absolute, p) => !p.is_positioned(),
            (Collect::Fixed, Position::Fixed) => {
                found.push(el);
                true // fixed descendants of a fixed box are the viewport's too
            }
            (Collect::Fixed, _) => true,
        };
        if descend {
            stack.extend(element_children(el).rev().map(|c| (c, depth + 1)));
        }
    }
    found
}

fn element_children(el: ElementRef<'_>) -> impl DoubleEndedIterator<Item = ElementRef<'_>> {
    el.children().filter_map(|c| match c {
        ChildRef::Element(e) => Some(e),
        ChildRef::Text(_) => None,
    })
}

/// Lays out one out-of-flow element in `cb` and places it: an inset that
/// is set positions that edge; with neither inset on an axis, the element
/// stays at its static position; with both (and an `auto` size), the size
/// is what the insets leave.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place(
    element: ElementRef<'_>,
    cb: ContainingBlock,
    origins: &std::collections::HashMap<dom::NodeId, (f32, f32)>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> LayoutBox {
    let style = style_of(style_cache, element);
    let (static_x, static_y) = reuse
        .static_position(element.node_id())
        .and_then(|p| origins.get(&p.container).map(|(x, y)| (x + p.dx, y + p.dy)))
        .unwrap_or((cb.x, cb.y));
    let inset = style.inset();
    let left = inset.left.map(|l| l.resolve(cb.width));
    let right = inset.right.map(|l| l.resolve(cb.width));
    let top = inset.top.map(|l| l.resolve(cb.height));
    let bottom = inset.bottom.map(|l| l.resolve(cb.height));
    let margin = style.margin.resolve(cb.width);

    // With both insets and an auto size, the insets decide the size.
    let forced_width = match (left, right, style.width.is_none() && style.width_intrinsic.is_none()) {
        (Some(l), Some(r), true) => Some((cb.width - l - r - margin.left - margin.right).max(0.0)),
        _ => None,
    };
    let forced_height = match (top, bottom, style.height.is_none()) {
        (Some(t), Some(b), true) => Some((cb.height - t - b - margin.top - margin.bottom).max(0.0)),
        _ => None,
    };
    let mut laid = layout_element_sized(
        element,
        0.0,
        0.0,
        cb.width,
        forced_width,
        forced_height,
        font,
        style_cache,
        intrinsic,
        image_cache,
        reuse,
        depth,
    );
    // Horizontal: `auto` margins with both insets and a set width centre
    // the box (§10.3.7); otherwise they are zero.
    let x = match (left, right) {
        (Some(l), Some(r)) if style.margin.left.is_none() && style.margin.right.is_none() => {
            cb.x + l + ((cb.width - l - r - laid.width) / 2.0).max(0.0)
        }
        (Some(l), _) => cb.x + l + margin.left,
        (None, Some(r)) => cb.x + cb.width - r - margin.right - laid.width,
        (None, None) => static_x + margin.left,
    };
    let y = match (top, bottom) {
        (Some(t), _) => cb.y + t + margin.top,
        (None, Some(b)) => cb.y + cb.height - b - margin.bottom - laid.height,
        (None, None) => static_y + margin.top,
    };
    let (dx, dy) = (x - laid.x, y - laid.y);
    translate(&mut laid, dx, dy);
    laid
}

/// Lays out and hoists into `container` every out-of-flow descendant it is
/// the containing block for (`kinds`), in tree order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_descendants(
    element: ElementRef<'_>,
    container: &mut LayoutBox,
    cb: ContainingBlock,
    kinds: &[Collect],
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) {
    if !reuse.saw_out_of_flow {
        return;
    }
    let found: Vec<ElementRef<'_>> = kinds.iter().flat_map(|&kind| collect(element, style_cache, kind)).collect();
    if found.is_empty() {
        return;
    }
    // Where each element's box is, to turn static positions (offsets from
    // a block container's box) into coordinates.
    let origins = box_origins(container);
    for el in found {
        let placed = place(el, cb, &origins, font, style_cache, intrinsic, image_cache, reuse, depth + 1);
        container.children.push(placed);
    }
}

/// The border-box origin of every element box in `root`'s subtree, in the
/// frame `root` is in (Milestone 39b: reused subtrees' frames are shifted).
fn box_origins(root: &LayoutBox) -> std::collections::HashMap<dom::NodeId, (f32, f32)> {
    let mut out = std::collections::HashMap::new();
    let mut stack = vec![crate::Placed::root(root)];
    while let Some(b) = stack.pop() {
        if let (Some(key), None) = (b.layout.element_key, &b.layout.text) {
            out.entry(key).or_insert((b.x(), b.y()));
        }
        stack.extend(b.children());
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::*;
    use css_parser::parse_stylesheet;
    use dom::Node;
    use images::ImageCache;

    /// `<body>` of `html`, and its layout with `css` on an 800px-wide
    /// viewport.
    fn laid(html: &str, css: &str) -> (dom::Element, LayoutBox) {
        let root = html_parser::parse_document(&format!("<html><body>{html}</body></html>")).unwrap();
        let body = root.children.iter().find_map(|n| match n { Node::Element(e) if e.tag == "body" => Some(e.clone()), _ => None }).unwrap();
        let laid = layout_document(&body, 800.0, &text::Font::dejavu_sans(), &parse_stylesheet(css), &mut ImageCache::new());
        (body, laid)
    }

    fn find(b: &LayoutBox, id: dom::NodeId) -> Option<&LayoutBox> {
        if b.element_key == Some(id) && b.text.is_none() {
            return Some(b);
        }
        b.children.iter().find_map(|c| find(c, id))
    }

    fn by_id<'a>(doc: &(dom::Element, LayoutBox), laid: &'a LayoutBox, id: &str) -> &'a LayoutBox {
        let el = doc.0.find_by_id(id).unwrap();
        find(laid, el.node_id).unwrap_or_else(|| panic!("no box for #{id}"))
    }

    #[test]
    fn relative_offsets_move_the_box_and_its_contents_but_not_its_siblings() {
        let html = r#"<div id=a>first</div><div id=b>second</div>"#;
        let plain_doc = laid(html, "div { height: 20px }");
        let plain = &plain_doc.1;
        let moved_doc = laid(html, "div { height: 20px } #a { position: relative; left: 10px; top: 5px }");
        let moved = &moved_doc.1;
        let (a0, a1) = (by_id(&plain_doc, plain, "a"), by_id(&moved_doc, moved, "a"));
        assert_eq!((a1.x - a0.x, a1.y - a0.y), (10.0, 5.0));
        assert_eq!(a1.children[0].x - a0.children[0].x, 10.0, "its text moved too");
        assert_eq!(by_id(&plain_doc, plain, "b").y, by_id(&moved_doc, moved, "b").y, "the next sibling did not");
        let right_doc = laid(html, "div { height: 20px } #a { position: relative; right: 10px; bottom: 5px }");
        let right = &right_doc.1;
        assert_eq!(by_id(&right_doc, right, "a").x - a0.x, -10.0);
    }

    #[test]
    fn absolute_boxes_leave_the_flow_and_sit_in_their_containing_block() {
        let html = r#"<div id=cb><div id=before>x</div><div id=abs>abs</div><div id=after>y</div></div>"#;
        let css = "#cb { position: relative; width: 400px; height: 300px; margin-left: 50px; border: 2px solid } \
                   #before, #after { height: 20px } #abs { position: absolute; right: 10px; bottom: 20px; width: 100px; height: 30px }";
        let l_doc = laid(html, css);
        let l = &l_doc.1;
        let (cb, abs, after) = (by_id(&l_doc, l, "cb"), by_id(&l_doc, l, "abs"), by_id(&l_doc, l, "after"));
        assert_eq!(after.y, by_id(&l_doc, l, "before").y + 20.0, "the absolute box takes no space");
        // Padding box of #cb: x = 50 + 2, y = 0 + 2, 400 x 300 content.
        assert_eq!(abs.x, cb.x + 2.0 + 400.0 - 10.0 - 100.0);
        assert_eq!(abs.y, cb.y + 2.0 + 300.0 - 20.0 - 30.0);
        assert_eq!(abs.children[0].text.as_ref().unwrap().content, "abs");
    }

    #[test]
    fn insets_on_both_sides_size_an_auto_box_and_auto_width_shrinks_to_fit() {
        let html = r#"<div id=cb><div id=stretch>s</div><div id=shrink>short</div></div>"#;
        let css = "#cb { position: relative; width: 400px; height: 200px } \
                   #stretch { position: absolute; left: 10px; right: 30px; top: 5px; bottom: 15px } \
                   #shrink { position: absolute; left: 0; top: 0 }";
        let l_doc = laid(html, css);
        let l = &l_doc.1;
        let stretch = by_id(&l_doc, l, "stretch");
        assert_eq!((stretch.width, stretch.height), (360.0, 180.0));
        assert!(by_id(&l_doc, l, "shrink").width < 100.0, "shrink-to-fit, not 400px");
    }

    #[test]
    fn with_no_insets_an_absolute_box_stays_at_its_static_position() {
        let html = r#"<div id=a>x</div><div id=abs>abs</div>"#;
        let l_doc = laid(html, "#a { height: 40px } #abs { position: absolute }");
        let l = &l_doc.1;
        assert_eq!(by_id(&l_doc, l, "abs").y, by_id(&l_doc, l, "a").y + 40.0);
    }

    #[test]
    fn without_a_positioned_ancestor_the_viewport_is_the_containing_block() {
        let html = r#"<div style="margin: 30px"><div id=abs>x</div><div id=fixed>f</div></div>"#;
        let l_doc = laid(html, "#abs { position: absolute; left: 5px; top: 6px } #fixed { position: fixed; right: 0; bottom: 0; width: 10px; height: 10px }");
        let l = &l_doc.1;
        let abs = by_id(&l_doc, l, "abs");
        assert_eq!((abs.x, abs.y), (5.0, 6.0));
        let fixed = by_id(&l_doc, l, "fixed");
        assert_eq!((fixed.x, fixed.y), (790.0, style::DEFAULT_VIEWPORT_HEIGHT - 10.0));
        assert_eq!(fixed.extras().positioning.scheme, style::Position::Fixed);
    }

    #[test]
    fn a_fixed_box_inside_a_positioned_ancestor_still_uses_the_viewport() {
        let html = r#"<div id=cb><div id=fixed>f</div></div>"#;
        let l_doc = laid(html, "#cb { position: relative; margin-left: 100px } #fixed { position: fixed; left: 0; top: 0 }");
        let l = &l_doc.1;
        let fixed = by_id(&l_doc, l, "fixed");
        assert_eq!((fixed.x, fixed.y), (0.0, 0.0));
    }

    #[test]
    fn z_index_decides_paint_and_hit_order() {
        let html = r#"<div id=cb><div id=low>low</div><div id=high>high</div><div id=flow>flow</div></div>"#;
        let css = "#cb { position: relative; height: 100px } \
                   #low, #high { position: absolute; left: 0; top: 0; width: 50px; height: 50px } \
                   #low { z-index: 2 } #high { z-index: 1 } #flow { height: 50px }";
        let l_doc = laid(html, css);
        let l = &l_doc.1;
        let cb = by_id(&l_doc, l, "cb");
        let order: Vec<Option<dom::NodeId>> = paint_order(&cb.children).map(|b| b.element_key).collect();
        let key = |id: &str| by_id(&l_doc, l, id).element_key;
        assert_eq!(order, vec![key("flow"), key("high"), key("low")], "in-flow first, then by z-index");
        let hit = hit_test_path(l, 10.0, 10.0);
        assert_eq!(hit.first().copied(), key("low"), "the highest z-index is hit");
    }

    #[test]
    fn a_negative_z_index_paints_below_in_flow_content() {
        let html = r#"<div id=cb><div id=under>u</div><div id=flow>f</div></div>"#;
        let l_doc = laid(html, "#cb { position: relative } #under { position: absolute; z-index: -1; top: 0; left: 0; width: 10px; height: 10px }");
        let l = &l_doc.1;
        let cb = by_id(&l_doc, l, "cb");
        assert_eq!(paint_order(&cb.children).next().unwrap().element_key, by_id(&l_doc, l, "under").element_key);
    }

    #[test]
    fn sticky_boxes_stay_in_view_within_their_container() {
        let html = r#"<div id=wrap><div id=head>h</div><div id=body>b</div></div><div id=after>a</div>"#;
        let l_doc = laid(html, "#wrap { height: 500px } #head { position: sticky; top: 0; height: 20px } #body { height: 100px } #after { height: 2000px }");
        let l = &l_doc.1;
        let head = by_id(&l_doc, l, "head");
        assert_eq!(head.sticky_offset(0.0, 0.0), 0.0);
        assert_eq!(head.sticky_offset(0.0, 100.0), 100.0 - head.y, "pinned to the top");
        let wrap = by_id(&l_doc, l, "wrap");
        let max = wrap.y + wrap.height - (head.y + head.height);
        assert_eq!(head.sticky_offset(0.0, 10_000.0), max, "never leaves its container");
        // Hit testing agrees: scrolled by 100, the pinned header is at the top.
        let hit = hit_test_path_scrolled(l, 5.0, 5.0, 100.0, &ScrollOffsets::new());
        assert_eq!(hit.first().copied(), head.element_key);
    }

    #[test]
    fn a_fixed_box_is_hit_where_it_is_drawn_after_scrolling() {
        let html = r#"<div id=bar>bar</div><div id=tall>t</div>"#;
        let l_doc = laid(html, "#bar { position: fixed; top: 0; left: 0; width: 100px; height: 30px } #tall { height: 5000px }");
        let l = &l_doc.1;
        let bar = by_id(&l_doc, l, "bar").element_key;
        assert_eq!(hit_test_path_scrolled(l, 10.0, 10.0, 1000.0, &ScrollOffsets::new()).first().copied(), bar);
        assert_ne!(hit_test_path_scrolled(l, 10.0, 1010.0, 1000.0, &ScrollOffsets::new()).first().copied(), bar);
    }

    #[test]
    fn an_absolute_box_outside_its_parent_is_still_hit() {
        let html = r#"<div id=cb><div id=menu>menu</div></div>"#;
        let l_doc = laid(html, "#cb { position: relative; height: 20px; width: 100px } #menu { position: absolute; top: 40px; left: 0; width: 100px; height: 50px }");
        let l = &l_doc.1;
        let menu = by_id(&l_doc, l, "menu").element_key;
        assert_eq!(hit_test_path(l, 10.0, 60.0).first().copied(), menu);
    }

    #[test]
    fn clipping_and_container_scroll_in_hit_testing() {
        let html = r#"<div id=box><div id=inner>i</div></div>"#;
        let css = "#box { overflow: auto; height: 50px; width: 100px } #inner { height: 200px }";
        let doc = laid(html, css);
        let l = &doc.1;
        let (bx, inner) = (by_id(&doc, l, "box"), by_id(&doc, l, "inner"));
        assert!(bx.overflow.scrollable);
        // Below the box (inside #inner's own overflowing extent): not hit.
        assert_ne!(hit_test_path(l, 10.0, 100.0).first().copied(), inner.element_key);
        let mut scrolls = ScrollOffsets::new();
        scrolls.insert(bx.element_key.unwrap(), (0.0, 120.0));
        let path = hit_test_path_scrolled(l, 10.0, 10.0, 0.0, &scrolls);
        assert_eq!(path.first().copied(), inner.element_key);
        assert_eq!(bx.scroll_range(), (0.0, 150.0));
    }
}
