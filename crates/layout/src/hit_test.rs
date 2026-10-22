//! Milestone 15: mapping a click position (viewport pixels) to the DOM
//! element(s) it hit, in event-dispatch bubble order.

use crate::LayoutBox;

/// Given a click position (viewport pixels, same space `LayoutBox`
/// coordinates live in), returns the "bubble path" of
/// `dom::element_ptr_key`s from the deepest matching element up
/// through its ancestors to the root — exactly the order event
/// dispatch needs (`js::JsEnvironment::dispatch_event` walks this
/// list in order, checking each for registered listeners, giving
/// bubbling-phase propagation for free from the order alone).
///
/// A word box's own `element_key` (see `InlineWord::element_key`'s doc
/// comment) is the innermost INLINE element it came from — `<a>`,
/// `<b>`, `<span>`, etc. — so clicking rendered link/inline text
/// resolves to that element specifically, not just its containing
/// block. Plain text with no inline wrapper (e.g. `<p>hello</p>`'s
/// "hello") has no word-level key of its own; the recursion pushes a
/// key only when a node HAS one, so that case simply contributes
/// nothing at the word level and the path starts from the nearest
/// real ancestor instead — the click still correctly attributes to
/// "the paragraph/div the text is in," matching how real browsers
/// attribute a click on plain rendered text to its containing element.
///
/// Later-painted (topmost, visually on top) boxes are checked first —
/// Milestone 33: in reverse [`crate::paint_order`], so `z-index` and
/// positioned boxes decide what is on top. Descendants are checked even
/// outside their parent's box (content overflows visibly, and an
/// absolutely positioned box is often outside its containing block).
///
/// `y` is in layout coordinates (document space, scroll already added).
/// Returns an empty `Vec` if the position doesn't hit anything.
///
/// Milestone 39b (ADR 022): a subtree whose recorded bounds miss the point
/// is skipped, and children ordered by position are binary-searched — a
/// click examines O(depth) boxes, not the page.
pub fn hit_test_path(root: &LayoutBox, x: f32, y: f32) -> Vec<dom::NodeId> {
    let mut walk = Walk { scroll: 0.0, containers: &crate::ScrollOffsets::new(), hit: Hit::default() };
    walk.visit(root, x, y, (0.0, 0.0), false);
    walk.hit.path
}

/// [`hit_test_path`] for a point in VIEWPORT coordinates on a page scrolled
/// by `scroll`: `fixed` boxes are hit where they are drawn (they do not
/// scroll), `sticky` ones where their sticky offset has moved them.
///
/// Milestone 33: a clipping box (`overflow` other than `visible`) passes
/// only points inside its padding box on to its descendants, and a scroll
/// container's `container_scrolls` offset moves them.
pub fn hit_test_path_scrolled(root: &LayoutBox, x: f32, viewport_y: f32, scroll: f32, container_scrolls: &crate::ScrollOffsets) -> Vec<dom::NodeId> {
    hit_test_counted(root, x, viewport_y, scroll, container_scrolls).path
}

/// Milestone 39b: what [`hit_test_counted`] found.
#[derive(Debug, Default)]
pub struct Hit<'a> {
    /// The bubble path (see [`hit_test_path`]).
    pub path: Vec<dom::NodeId>,
    /// Per element of `path`, its outermost box the point hit, placed —
    /// so nothing needs searching the tree for it.
    pub boxes: Vec<crate::Placed<'a>>,
    /// Boxes examined (tests bound a click's work with it).
    pub examined: usize,
}

/// [`hit_test_path_scrolled`], also giving the boxes hit and how many boxes
/// it examined.
pub fn hit_test_counted<'a>(root: &'a LayoutBox, x: f32, viewport_y: f32, scroll: f32, container_scrolls: &crate::ScrollOffsets) -> Hit<'a> {
    let mut walk = Walk { scroll, containers: container_scrolls, hit: Hit::default() };
    walk.visit(root, x, viewport_y + scroll, (0.0, 0.0), false);
    walk.hit
}

/// Below this many children, a scan is as cheap as a search.
const SEARCH_CHILDREN: usize = 16;

struct Walk<'a, 't> {
    /// The page's scroll offset.
    scroll: f32,
    containers: &'a crate::ScrollOffsets,
    hit: Hit<'t>,
}

impl<'t> Walk<'_, 't> {
    /// Whether (`x`, `y`) — in the frame `node` is in, whose origin in the
    /// document is `origin` — hits `node` or a descendant; adds the hit's
    /// elements to the path.
    fn visit(&mut self, node: &'t LayoutBox, x: f32, y: f32, origin: (f32, f32), in_fixed: bool) -> bool {
        self.hit.examined += 1;
        if let Some((x0, y0, x1, y1)) = node.subtree.bounds {
            if !(x >= x0 && x <= x1 && y >= y0 && y <= y1) {
                return false;
            }
        }
        let children_reachable = match node.clip_rect() {
            Some((x0, y0, x1, y1)) => x >= x0 && x <= x1 && y >= y0 && y <= y1,
            None => true,
        };
        if children_reachable && !node.children.is_empty() {
            let (sx, sy) = node
                .element_key
                .filter(|_| node.overflow.scrollable)
                .and_then(|k| self.containers.get(&k).copied())
                .unwrap_or((0.0, 0.0));
            // The point in the children's frame.
            let (ox, oy) = node.subtree.child_offset;
            if self.visit_children(node, x + sx - ox, y + sy - oy, (origin.0 + ox, origin.1 + oy), in_fixed) {
                self.push(node, origin);
                return true;
            }
        }
        if !node.visible || x < node.x || x > node.x + node.width || y < node.y || y > node.y + node.height {
            return false;
        }
        self.push(node, origin);
        true
    }

    /// Adds a box's element to the path. Generated content (`::before`,
    /// `::after`) stands for its element, which is added once; the
    /// element's outermost box hit is the one kept.
    fn push(&mut self, node: &'t LayoutBox, origin: (f32, f32)) {
        let Some(key) = node.element_key.map(style::owner_of) else { return };
        let placed = crate::Placed { layout: node, origin };
        if self.hit.path.last() == Some(&key) {
            if node.element_key == Some(key) {
                *self.hit.boxes.last_mut().expect("one box per path entry") = placed;
            }
        } else {
            self.hit.path.push(key);
            self.hit.boxes.push(placed);
        }
    }

    fn visit_children(&mut self, node: &'t LayoutBox, x: f32, y: f32, origin: (f32, f32), in_fixed: bool) -> bool {
        let children = &node.children[..];
        if node.subtree.ordered && children.len() >= SEARCH_CHILDREN {
            // In tree order (= paint order: none is positioned), both
            // vertical edges of the children's bounds only grow: those that
            // can hold `y` are one run, found by two searches.
            let edges = |c: &LayoutBox| c.subtree.bounds.map_or((f32::NEG_INFINITY, f32::INFINITY), |(_, y0, _, y1)| (y0, y1));
            let first = children.partition_point(|c| edges(c).1 < y);
            let end = children.partition_point(|c| edges(c).0 <= y).max(first);
            return children[first..end].iter().rev().any(|child| self.visit(child, x, y, origin, in_fixed));
        }
        for child in crate::paint_order(children).rev() {
            let fixed = child.position_scheme() == style::Position::Fixed && !in_fixed;
            let child_y = if fixed {
                y - self.scroll
            } else if child.position_scheme() == style::Position::Sticky {
                y - child.sticky_offset(origin.1, self.scroll)
            } else {
                y
            };
            if self.visit(child, x, child_y, origin, in_fixed || fixed) {
                return true;
            }
        }
        false
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



    // ---- Milestone 15: hit-testing ----

    #[test]
    fn hit_test_finds_the_element_at_a_point() {
        let mut root = Element::new("div");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "width:100px;height:50px".into()));
        a.attributes.push(("id".into(), "a".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "width:100px;height:50px".into()));
        b.attributes.push(("id".into(), "b".into()));
        root.children.push(Node::Element(a));
        root.children.push(Node::Element(b));

        let laid_out = layout_document(&root, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let hit_a = hit_test_path(&laid_out, 10.0, 10.0);
        let hit_b = hit_test_path(&laid_out, 10.0, 60.0);

        let a_key = laid_out.children[0].element_key.unwrap();
        let b_key = laid_out.children[1].element_key.unwrap();
        assert_eq!(hit_a[0], a_key);
        assert_eq!(hit_b[0], b_key);
    }


    #[test]
    fn hit_test_outside_all_boxes_returns_empty() {
        let el = Element::new("div");
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let hit = hit_test_path(&laid_out, 99999.0, 99999.0);
        assert!(hit.is_empty());
    }


    #[test]
    fn hit_test_path_includes_all_ancestors_in_bubble_order() {
        let mut outer = Element::new("div");
        outer.attributes.push(("id".into(), "outer".into()));
        outer.attributes.push(("style".into(), "width:200px;height:200px".into()));
        let mut inner = Element::new("div");
        inner.attributes.push(("id".into(), "inner".into()));
        inner.attributes.push(("style".into(), "width:50px;height:50px".into()));
        outer.children.push(Node::Element(inner));

        let laid_out = layout_document(&outer, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let inner_key = laid_out.children[0].element_key.unwrap();
        let outer_key = laid_out.element_key.unwrap();

        let hit = hit_test_path(&laid_out, 5.0, 5.0);
        assert_eq!(hit, vec![inner_key, outer_key]);
    }


    #[test]
    fn hit_on_plain_text_with_no_inline_wrapper_bubbles_to_the_containing_element() {
        // Plain text with no inline element wrapping it has
        // element_key: None on its own word box — a click landing on
        // it must still resolve to the containing <p>, not nothing.
        let mut p = Element::new("p");
        p.attributes.push(("id".into(), "para".into()));
        p.children.push(Node::Text("hello".to_string()));

        let laid_out = layout_document(&p, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let para_key = laid_out.element_key.unwrap();

        let word_box = &laid_out.children[0];
        let hit = hit_test_path(&laid_out, word_box.x + 1.0, word_box.y + 1.0);
        assert_eq!(hit[0], para_key);
    }


    #[test]
    fn hit_on_a_link_inside_a_paragraph_resolves_to_the_link_not_just_the_paragraph() {
        // The exact real-world scenario Milestone 21's demo uses (and
        // that this test exists BECAUSE a real human clicking it in
        // the actual app found broken): `<p>text <a href="...">link
        // text</a>.</p>` — clicking the rendered LINK text must name
        // the <a> as the click target, not just bubble straight to
        // the paragraph the way plain unwrapped text does. Before
        // `InlineWord::element_key` existed, every inline word box's
        // `element_key` was unconditionally `None`, so `hit_test_path`
        // could NEVER put an `<a>` (or any other inline element) in
        // the returned path at all — `dispatch_click`'s `hit_path.first()
        // .tag == "a"` check would always fail, and clicking a link
        // would silently do nothing.
        let mut p = Element::new("p");
        p.attributes.push(("id".into(), "para".into()));
        p.children.push(Node::Text("click this: ".to_string()));
        let mut a = Element::new("a");
        a.attributes.push(("id".into(), "the-link".into()));
        a.attributes.push(("href".into(), "https://example.com/".into()));
        a.children.push(Node::Text("a real link".to_string()));
        p.children.push(Node::Element(a));

        let laid_out = layout_document(&p, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let para_key = laid_out.element_key.unwrap();

        // The link's word boxes are whichever children came from "a
        // real link" — find one by checking its rendered text, rather
        // than assuming a fixed index (robust to how many word boxes
        // the plain lead-in text produced).
        let link_word_box = laid_out
            .children
            .iter()
            .find(|b| b.text.as_ref().is_some_and(|t| t.content == "a"))
            .expect("the link's first word box should exist");
        let link_key = link_word_box.element_key.expect("a word box from inside an <a> must carry that <a>'s element_key");
        assert_ne!(link_key, para_key, "the link's key must be its OWN element, not the paragraph's");

        let hit = hit_test_path(&laid_out, link_word_box.x + 1.0, link_word_box.y + 1.0);
        assert_eq!(hit[0], link_key, "clicking the link's rendered text must resolve to the <a>, not just the paragraph");
        assert_eq!(hit[1], para_key, "must still bubble up to the paragraph after the link, for capture/bubble dispatch");
    }


    #[test]
    fn hit_test_iterates_children_in_reverse_for_stacking_order() {
        // Real overlapping siblings aren't achievable without
        // `position: absolute` (not supported — see architecture
        // doc's deferred-features ledger), so this checks the
        // mechanism directly: `hit_test_recursive` iterates children
        // via `.rev()`, meaning a LATER document-order child is
        // checked (and would win) BEFORE an earlier one at the same
        // nesting level. Confirmed here via two children where only
        // the later one's box actually contains the point — proving
        // the later child isn't skipped due to iteration order.
        let mut root = Element::new("div");
        let mut first = Element::new("div");
        first.attributes.push(("id".into(), "first".into()));
        first.attributes.push(("style".into(), "width:50px;height:50px".into()));
        let mut second = Element::new("div");
        second.attributes.push(("id".into(), "second".into()));
        second.attributes.push(("style".into(), "width:50px;height:50px".into()));
        root.children.push(Node::Element(first));
        root.children.push(Node::Element(second));

        let laid_out = layout_document(&root, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let second_key = laid_out.children[1].element_key.unwrap();
        // second occupies y=[50,100) (stacked below first).
        let hit = hit_test_path(&laid_out, 5.0, 60.0);
        assert_eq!(hit[0], second_key);
    }

}
