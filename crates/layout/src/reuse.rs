//! Layout subtree reuse — Milestone 31 (`docs/adr/008-dirty-tracking-and-identity.md`).
//!
//! An element's laid-out subtree depends only on the element's own subtree
//! (its styles, text, images) and on the width it is laid out in — not on
//! where it is placed. So a subtree nothing changed can be taken from the
//! previous frame's layout and **translated** to its new position instead of
//! being laid out again: a change deep in a page re-lays out the changed
//! element and its ancestor chain, and reuses everything else.
//!
//! What is kept between frames is the previous layout tree itself, plus, per
//! element, how it was laid out (width, position, depth) and where its box
//! sits (parent element, child index) — O(elements) memory. A reused subtree
//! is *moved* out of the previous tree, not cloned (the previous tree is
//! discarded after the frame).
//!
//! Milestone 39b (ADR 022): moving it is O(1) — its root's position and the
//! offset of its children's frame change, nothing inside it — and so is its
//! bookkeeping: each element's entry is in the frame its box is in and is
//! kept until the element is laid out again, so a moved subtree's entries
//! need no copying. Entries are stamped with the frame that wrote them.
//!
//! The caller says what may not be reused ([`LayoutReuse::begin_frame`]:
//! every element that was mutated or whose final style changed, and all
//! their ancestors). Flex items are never reused — a flex container
//! re-positions and stretches its items after laying them out, so their
//! boxes in the previous tree are not what laying them out returned.
//!
//! Correctness is checked like the incremental restyle: incremental layout
//! equals a fresh layout over random mutation sequences.

use crate::LayoutBox;
use dom::{ChildRef, ElementRef, NodeId};
use dom::{NodeIdMap, NodeIdSet};

/// Where an out-of-flow element would have been: an offset from its block
/// container's border box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StaticPosition {
    pub container: NodeId,
    pub dx: f32,
    pub dy: f32,
}

/// How an element was last laid out. A reuse needs the same width and depth
/// (the depth cap can cut a subtree short).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Meta {
    available_width: f32,
    /// Where it was laid out, in the frame its box is in (ADR 022).
    outer_x: f32,
    outer_y: f32,
    depth: usize,
    /// Milestone 33: laid out beside floats — its lines depend on where
    /// they were, so it is never reused.
    beside_floats: bool,
    /// Milestone 39c: its bottom margin (a patch must keep it).
    margin_bottom: f32,
    /// The frame that wrote this entry.
    frame: u32,
}

/// Milestone 39c: how an element was last laid out (see
/// [`LayoutReuse::patch_place`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PatchPlace {
    pub outer_x: f32,
    pub outer_y: f32,
    pub available_width: f32,
    pub depth: usize,
    pub margin_bottom: f32,
}

/// Where an element's box sits in the layout tree: its parent element's box
/// and its index among that box's children (`None`: the root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    parent: Option<NodeId>,
    index: u32,
    /// The frame that wrote this entry. An element's link and meta are
    /// written in the same frame when its box is placed in a parent's
    /// children; a box placed some other way (a flex item, a table cell)
    /// leaves its link older than its meta, and is never found by its link.
    frame: u32,
}

/// Previous-frame layout, and what may not be reused this frame.
#[derive(Debug, Default)]
pub struct LayoutReuse {
    previous_tree: Option<LayoutBox>,
    /// Kept across frames (ADR 022): see the module comment.
    meta: NodeIdMap<Meta>,
    links: NodeIdMap<Link>,
    /// The current frame (stamps entries); 0 before the first.
    frame: u32,
    dirty: NodeIdSet,
    /// Elements laid out fresh this frame.
    pub laid_out: usize,
    /// Boxes taken (moved) from the previous frame's layout.
    pub reused: usize,
    /// Milestone 39b: box updates spent moving them — one per subtree.
    pub moved_box_updates: usize,
    /// Milestone 33: where each out-of-flow (absolute/fixed) element would
    /// have been in flow — its static position — as an offset from the
    /// border box of the block container it was in. Relative to that box,
    /// so it stays right however the box is later moved (a relative offset,
    /// line placement of an inline-block, flex placement, reuse).
    static_positions: NodeIdMap<StaticPosition>,
    /// The root's box, if it was taken from the previous frame this frame.
    root_taken: Option<NodeId>,
    /// The last parent whose children were taken this frame, and its box's
    /// path in the previous tree (root first): siblings share it.
    parent_path: Option<(NodeId, Vec<u32>)>,
    /// Milestone 39c: the element this frame's layout was patched at, and
    /// its box's path from the root (ADR 022 §8).
    patched: Option<(NodeId, Vec<u32>)>,
    /// Whether the page has had an out-of-flow element — until then no
    /// containing block needs searching for one.
    pub(crate) saw_out_of_flow: bool,
    /// The viewport: the initial containing block, and what `fixed` boxes
    /// are placed in.
    pub(crate) viewport: (f32, f32),
    /// Milestone 33: the floats line boxes currently flow around (see
    /// `floats.rs`).
    pub(crate) exclusions: Vec<crate::floats::Exclusion>,
    /// The intrinsic sizes measured so far (performance audit): kept
    /// across frames so a change re-measures only the dirty elements.
    pub(crate) intrinsic: crate::IntrinsicCache,
    /// Whether the page needs intrinsic sizing at all (the last check).
    pub(crate) intrinsic_needed: bool,
    /// Elements whose intrinsic sizes were measured this frame (the rest
    /// were reused).
    pub intrinsic_measured: usize,
}

impl LayoutReuse {
    pub fn new() -> Self {
        Self { viewport: (style::DEFAULT_VIEWPORT_WIDTH, style::DEFAULT_VIEWPORT_HEIGHT), ..Self::default() }
    }

    /// The viewport size — the initial containing block for positioned
    /// boxes. A change should come with a fresh layout (`begin_frame` with
    /// no previous tree): `fixed`/`sticky` boxes depend on it.
    pub fn set_viewport(&mut self, width: f32, height: f32) {
        self.viewport = (width, height);
    }

    /// Starts a frame. `previous_tree` is the last frame's layout (moved in:
    /// reused subtrees are taken out of it); `None` means nothing can be
    /// reused. `dirty` must hold every element that was mutated or whose
    /// final style changed, AND every ancestor of each.
    pub fn begin_frame(&mut self, previous_tree: Option<LayoutBox>, dirty: NodeIdSet) {
        self.frame += 1;
        self.root_taken = None;
        self.parent_path = None;
        self.patched = None;
        if previous_tree.is_none() {
            self.meta.clear();
            self.links.clear();
            self.static_positions.clear();
            self.saw_out_of_flow = false;
            // Nothing to be incremental against: measure afresh.
            self.intrinsic = crate::IntrinsicCache::new();
            self.intrinsic_needed = false;
        }
        self.previous_tree = previous_tree;
        self.dirty = dirty;
        self.laid_out = 0;
        self.reused = 0;
        self.moved_box_updates = 0;
        self.intrinsic_measured = 0;
    }

    /// Ends a frame: the previous tree (whatever was not reused out of it)
    /// is dropped.
    pub fn end_frame(&mut self) {
        // Drop the sizes of elements that are gone, amortised: only once
        // the cache is well past the number of elements laid out.
        if self.intrinsic.len() > 2 * self.meta.len() + 64 {
            let meta = &self.meta;
            self.intrinsic.retain(|n| meta.contains_key(&n) || meta.contains_key(&style::owner_of(n)));
        }
        self.previous_tree = None;
    }

    /// How many elements' entries are kept (see [`Self::retain_live`]).
    pub fn tracked(&self) -> usize {
        self.meta.len()
    }

    /// Drops the entries of elements that are gone (`live` says which are
    /// not): kept entries otherwise outlive removed elements. The caller
    /// does this when [`Self::tracked`] outgrows the document.
    pub fn retain_live(&mut self, live: impl Fn(NodeId) -> bool) {
        self.meta.retain(|&n, _| live(n));
        self.links.retain(|&n, _| live(n));
        self.static_positions.retain(|&n, _| live(n));
    }

    /// The previous frame's layout of `element`, moved to (`outer_x`,
    /// `outer_y`), if it may be reused here.
    pub(crate) fn take(&mut self, element: ElementRef<'_>, outer_x: f32, outer_y: f32, available_width: f32, depth: usize) -> Option<LayoutBox> {
        let node = element.node_id();
        // A generated `::before`/`::after` changes with its owner.
        if self.dirty.contains(&node) || self.dirty.contains(&style::owner_of(node)) {
            return None;
        }
        // Nothing is reused beside floats, nor what was laid out beside them.
        if !self.exclusions.is_empty() {
            return None;
        }
        let meta = *self.meta.get(&node)?;
        if meta.frame >= self.frame || meta.available_width != available_width || meta.depth != depth || meta.beside_floats {
            return None;
        }
        // The box's position in the previous tree: its link, then its
        // parent's path — the same for all its siblings, so found once per
        // parent (each link written by an earlier frame, with its element's
        // layout).
        let link = *self.links.get(&node)?;
        if link.frame != meta.frame || link.frame >= self.frame {
            return None;
        }
        if let Some(parent) = link.parent {
            if self.parent_path.as_ref().is_none_or(|(p, _)| *p != parent) {
                let mut path = self.path_of(parent, |frame| frame < self.frame)?;
                path.reverse();
                self.parent_path = Some((parent, path));
            }
        }
        let parent_path = match (link.parent, &self.parent_path) {
            (Some(_), Some((_, path))) => &path[..],
            _ => &[],
        };
        // Both positions are in the frame of the box's parent, old and new
        // (ADR 022): what lies between the old frame and the document
        // cancels out.
        let (dx, dy) = (outer_x - meta.outer_x, outer_y - meta.outer_y);
        let mut layout = take_box(self.previous_tree.as_mut()?, parent_path, link.parent.map(|_| link.index))?;
        if layout.element_key != Some(node) {
            return None; // not the box we expected: never guess
        }
        move_subtree(&mut layout, dx, dy);
        self.moved_box_updates += 1;
        self.reused += layout.subtree.boxes as usize;
        self.meta.insert(node, Meta { available_width, outer_x, outer_y, depth, beside_floats: false, margin_bottom: meta.margin_bottom, frame: self.frame });
        if link.parent.is_none() {
            self.root_taken = Some(node);
        }
        Some(layout)
    }

    /// The child indices from the root's box down to `node`'s, deepest
    /// first, if every element on the way was placed by its parent in the
    /// frame that last laid it out, and `stamp_ok` accepts that frame.
    fn path_of(&self, node: NodeId, stamp_ok: impl Fn(u32) -> bool) -> Option<Vec<u32>> {
        let mut path = Vec::new();
        let mut at = node;
        loop {
            let link = *self.links.get(&at)?;
            let meta = self.meta.get(&at)?;
            if link.frame != meta.frame || !stamp_ok(link.frame) {
                return None;
            }
            match link.parent {
                Some(parent) => {
                    path.push(link.index);
                    at = parent;
                }
                None => return Some(path),
            }
        }
    }

    /// Whether the root's box (`root`) was taken from the previous frame
    /// this frame.
    pub(crate) fn root_was_taken(&self, root: NodeId) -> bool {
        self.root_taken == Some(root)
    }

    /// Records an out-of-flow element's static position: (`dx`, `dy`) from
    /// the border box of `container`, the block it was in.
    pub(crate) fn record_static_position(&mut self, node: NodeId, container: NodeId, dx: f32, dy: f32) {
        self.saw_out_of_flow = true;
        self.static_positions.insert(node, StaticPosition { container, dx, dy });
    }

    /// An out-of-flow element's static position, if known.
    pub(crate) fn static_position(&self, node: NodeId) -> Option<StaticPosition> {
        self.static_positions.get(&node).copied()
    }

    /// `nodes` plus every ancestor of each, in `document` — the dirty set
    /// [`LayoutReuse::begin_frame`] needs (an ancestor's size can depend on
    /// any descendant's). Nodes no longer in the document are skipped.
    pub fn with_ancestors(document: &dom::Document, nodes: impl IntoIterator<Item = NodeId>) -> NodeIdSet {
        let mut dirty = NodeIdSet::default();
        for node in nodes {
            if !document.contains(node) || !dirty.insert(node) {
                continue;
            }
            // O(1) per step up the parent links; stop where the chain joins
            // one already added.
            let mut at = node;
            while let Some(parent) = document.parent_id(at) {
                if !dirty.insert(parent) {
                    break;
                }
                at = parent;
            }
        }
        dirty
    }

    /// Milestone 39c: the element this frame's layout patched into the
    /// previous tree, and its box's path (child indices from the root) —
    /// `None` when the frame laid out as usual.
    pub fn patched(&self) -> Option<(NodeId, &[u32])> {
        self.patched.as_ref().map(|(n, p)| (*n, &p[..]))
    }

    /// Milestone 39c: how `node` was last laid out — its outer position,
    /// width, depth and bottom margin — and its box's path from the root in
    /// the previous tree, if it may be patched there: laid out by an
    /// earlier frame, placed by its parent's block flow all the way up, not
    /// beside floats.
    pub(crate) fn patch_place(&self, node: NodeId) -> Option<(PatchPlace, Vec<u32>)> {
        let meta = *self.meta.get(&node)?;
        if meta.frame >= self.frame || meta.beside_floats {
            return None;
        }
        let mut path = self.path_of(node, |frame| frame < self.frame)?;
        path.reverse();
        let place = PatchPlace { outer_x: meta.outer_x, outer_y: meta.outer_y, available_width: meta.available_width, depth: meta.depth, margin_bottom: meta.margin_bottom };
        Some((place, path))
    }

    /// Milestone 39c: the previous frame's tree, to patch in place.
    pub(crate) fn previous_tree_mut(&mut self) -> Option<&mut LayoutBox> {
        self.previous_tree.as_mut()
    }

    /// Milestone 39c: ends a patch — the previous tree, patched, is this
    /// frame's layout; `node`'s link is written again (it was laid out this
    /// frame), at its place.
    pub(crate) fn finish_patch(&mut self, node: NodeId, path: Vec<u32>, parent: NodeId) -> Option<LayoutBox> {
        let index = *path.last()?;
        self.link(node, Some(parent), index as usize);
        let tree = self.previous_tree.take()?;
        // Every box outside the patched one was kept.
        let patched_boxes = path.iter().try_fold(&tree, |b, &i| b.children.get(i as usize)).map_or(0, |b| b.subtree.boxes);
        self.reused += tree.subtree.boxes.saturating_sub(patched_boxes) as usize;
        self.patched = Some((node, path));
        Some(tree)
    }

    /// The current frame's number (see `Subtree::frame`).
    pub(crate) fn frame(&self) -> u32 {
        self.frame
    }

    /// Whether this frame has a previous layout to reuse from.
    pub fn has_previous(&self) -> bool {
        self.previous_tree.is_some()
    }

    /// `node`'s box in `tree` — the layout this frame produced — found by
    /// its recorded position in O(depth), placed in the document (Milestone
    /// 39b), or `None` when not recorded.
    pub fn box_of<'t>(&self, tree: &'t LayoutBox, node: NodeId) -> Option<crate::Placed<'t>> {
        let path = self.path_of(node, |_| true)?;
        let mut found = crate::Placed::root(tree);
        for &i in path.iter().rev() {
            let origin = found.child_origin();
            found = crate::Placed { layout: found.layout.children.get(i as usize)?, origin };
        }
        // Never guess: the recorded position must hold this element's box.
        (found.layout.element_key == Some(node)).then_some(found)
    }

    /// M38b's gap (ADR 020): `node`'s border box in document space,
    /// `(x0, y0, x1, y1)`, when only a new layout can move it on screen —
    /// neither it nor an ancestor is transformed, fixed, sticky or in a
    /// scroll container, all of which move without one. `None` when that
    /// cannot be said (the box was not recorded, or one of those holds).
    pub fn still_rect_of(&self, tree: &LayoutBox, node: NodeId) -> Option<(f32, f32, f32, f32)> {
        let still = |b: &LayoutBox| {
            !b.overflow.scrollable
                && b.extras.as_ref().is_none_or(|e| {
                    e.transform == style::Transform::default() && !matches!(e.positioning.scheme, style::Position::Fixed | style::Position::Sticky)
                })
        };
        let path = self.path_of(node, |_| true)?;
        let mut found = crate::Placed::root(tree);
        if !still(found.layout) {
            return None;
        }
        for &i in path.iter().rev() {
            let origin = found.child_origin();
            found = crate::Placed { layout: found.layout.children.get(i as usize)?, origin };
            if !still(found.layout) {
                return None;
            }
        }
        (found.layout.element_key == Some(node)).then(|| (found.x(), found.y(), found.x() + found.layout.width, found.y() + found.layout.height))
    }

    /// The elements this frame may not reuse (see `begin_frame`).
    pub(crate) fn dirty(&self) -> &NodeIdSet {
        &self.dirty
    }

    /// Records how `node` was laid out this frame.
    pub(crate) fn store(&mut self, node: NodeId, outer_x: f32, outer_y: f32, available_width: f32, depth: usize, margin_bottom: f32) {
        self.laid_out += 1;
        let beside_floats = !self.exclusions.is_empty();
        self.meta.insert(node, Meta { available_width, outer_x, outer_y, depth, beside_floats, margin_bottom, frame: self.frame });
    }

    /// Records that `child`'s box is child number `index` of `parent`'s box
    /// (`parent == None`: `child` is the root).
    pub(crate) fn link(&mut self, child: NodeId, parent: Option<NodeId>, index: usize) {
        self.links.insert(child, Link { parent, index: index as u32, frame: self.frame });
        // Its path in the previous tree no longer follows from its link.
        if self.parent_path.as_ref().is_some_and(|(p, _)| *p == child) {
            self.parent_path = None;
        }
    }

    /// Milestone 33: `element`'s laid-out subtree was moved by (`dx`, `dy`)
    /// after its descendants were laid out (a `position: relative`
    /// offset): their recorded layout positions move with it, so a later
    /// reuse of any of them starts from where its box really is.
    pub(crate) fn shift_descendants(&mut self, element: ElementRef<'_>, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for id in descendant_ids(element) {
            if let Some(meta) = self.meta.get_mut(&id) {
                meta.outer_x += dx;
                meta.outer_y += dy;
            }
        }
    }

}

/// The ids of every box-generating descendant of `element`: its element
/// descendants and (Milestone 33) the generated `::before`/`::after` of
/// it and of each of them.
fn descendant_ids(element: ElementRef<'_>) -> Vec<NodeId> {
    let pseudos = |id: NodeId| [style::pseudo_id(id, style::PseudoElement::Before), style::pseudo_id(id, style::PseudoElement::After)];
    let mut out: Vec<NodeId> = pseudos(element.node_id()).to_vec();
    let mut stack: Vec<ElementRef<'_>> = element_children(element).collect();
    while let Some(el) = stack.pop() {
        out.push(el.node_id());
        out.extend(pseudos(el.node_id()));
        stack.extend(element_children(el));
    }
    out
}

fn element_children(el: ElementRef<'_>) -> impl Iterator<Item = ElementRef<'_>> {
    el.children().filter_map(|c| match c {
        ChildRef::Element(e) => Some(e),
        ChildRef::Text(_) => None,
    })
}

/// Moves the box at `parent_path` + `index` (none: the box at
/// `parent_path` itself) out of `root`, leaving an empty box in its place
/// (the previous tree is discarded after the frame; its indices stay valid).
fn take_box(root: &mut LayoutBox, parent_path: &[u32], index: Option<u32>) -> Option<LayoutBox> {
    let mut node = root;
    for &i in parent_path {
        node = node.children.get_mut(i as usize)?;
    }
    let slot = match index {
        Some(i) => node.children.get_mut(i as usize)?,
        None => node,
    };
    Some(std::mem::replace(slot, LayoutBox::empty()))
}

/// Moves a laid-out subtree by (`dx`, `dy`), box by box — how layout places
/// what it has just built (a fresh layout's positions stay exactly what
/// they were computed as). Iterative: layout trees can be as deep as the
/// layout depth cap.
pub(crate) fn translate(root: &mut LayoutBox, dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        shift_box(node, dx, dy);
        stack.extend(node.children.iter_mut());
    }
}

/// Milestone 39b (ADR 022): moves a subtree taken from the previous frame
/// in O(1) — its root, and the frame its children are in.
fn move_subtree(root: &mut LayoutBox, dx: f32, dy: f32) {
    shift_box(root, dx, dy);
    root.subtree.child_offset.0 += dx;
    root.subtree.child_offset.1 += dy;
}

/// Moves one box (not its children) by (`dx`, `dy`) in its parent's frame.
fn shift_box(node: &mut LayoutBox, dx: f32, dy: f32) {
    node.x += dx;
    node.y += dy;
    if let Some(text) = &mut node.text {
        text.baseline_y += dy;
    }
    if let Some((x0, y0, x1, y1)) = &mut node.subtree.bounds {
        *x0 += dx;
        *x1 += dx;
        *y0 += dy;
        *y1 += dy;
    }
}

/// Milestone 39b (ADR 022): computes `subtree` (bounds, order, box count)
/// for every box whose bounds are unknown — the boxes laid out (or changed)
/// this frame — and stops at the rest, which carry theirs.
pub(crate) fn finish_subtrees(node: &mut LayoutBox, frame: u32) {
    if node.subtree.bounds.is_some() {
        return;
    }
    node.subtree.frame = frame;
    let (ox, oy) = node.subtree.child_offset;
    let mut boxes = 1u32;
    let mut ordered = true;
    let mut moves_with_scroll = false;
    let mut children: Option<(f32, f32, f32, f32)> = None;
    let mut previous: Option<(f32, f32)> = None;
    for child in &mut node.children {
        finish_subtrees(child, frame);
        boxes = boxes.saturating_add(child.subtree.boxes);
        let scheme = child.position_scheme();
        moves_with_scroll |= matches!(scheme, style::Position::Fixed | style::Position::Sticky);
        let Some((x0, y0, x1, y1)) = child.subtree.bounds else { unreachable!("just computed") };
        if scheme.is_positioned() || previous.is_some_and(|(p0, p1)| y0 < p0 || y1 < p1) {
            ordered = false;
        }
        previous = Some((y0, y1));
        children = Some(union(children, (x0 + ox, y0 + oy, x1 + ox, y1 + oy)));
    }
    let own = (node.x, node.y, node.x + node.width, node.y + node.height);
    const ALL: (f32, f32, f32, f32) = (f32::NEG_INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::INFINITY);
    let reach = if node.overflow.x && node.overflow.y {
        // Descendants are hit only inside the padding box, which the
        // border box contains — however the content is scrolled.
        None
    } else if moves_with_scroll || (node.overflow.clips() && node.overflow.scrollable) {
        // Where a fixed or sticky box (or content scrolled along an axis
        // that does not clip) is depends on scrolling.
        Some(ALL)
    } else {
        match (children, node.clip_rect()) {
            (Some((x0, y0, x1, y1)), Some((c0, d0, c1, d1))) => Some((x0.max(c0), y0.max(d0), x1.min(c1), y1.min(d1))),
            (c, _) => c,
        }
    };
    node.subtree.bounds = Some(match reach {
        Some(r) if !(r.0 > r.2 || r.1 > r.3) => union(Some(own), r),
        _ => own,
    });
    node.subtree.ordered = ordered;
    node.subtree.boxes = boxes;
}

pub(crate) fn union(acc: Option<(f32, f32, f32, f32)>, b: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    match acc {
        None => b,
        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::{Element, Node};
    use crate::{layout_document_incremental, layout_document_with_style_cache};
    use dom::{Document, Mutations};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const SHEET: &str = ".w { width: 120px; } .h { height: 13px; } .p { padding: 3px; margin: 2px; } .f { display: flex; } \
        .big { font-size: 20px; } span.b { font-size: 9px; } .bd { border: 2px solid #000000; } .c { text-align: center; } \
        .n { display: none; } .ib { display: inline-block; padding: 1em; } .i { display: inline; } .if { display: inline-flex; } \
        .m { margin: 7px 0 11px; } .e { width: 50%; margin: 0 auto; } .mx { max-width: 90px; margin: 0 auto; } \
        .g { flex-grow: 1; min-width: 0; } .r { position: relative; top: 3px; left: 4px; } \
        .a { position: absolute; top: 5px; right: 6px; width: 40px; } .x { position: absolute; } .s { position: sticky; top: 0; } \
        .fx { position: fixed; bottom: 0; left: 0; } .o { overflow: hidden; height: 20px; } .oa { overflow: auto; max-height: 30px; } \
        .gb::before { content: 'b4 ' } .ga::after { content: 'after'; display: block; height: 5px } .gi::before { content: ''; display: inline-block; width: 9px } \
        .fl { float: left; width: 60px; } .fr { float: right; } .cl { clear: both; } \
        .tb { display: table; border-spacing: 3px; } .trw { display: table-row; } .tc { display: table-cell; padding: 2px; } \
        .gr { display: grid; grid-template-columns: repeat(auto-fill, minmax(40px, 1fr)); gap: 3px; } .gs { grid-column: span 2; } \
        .ga { display: grid; grid-template-columns: 30px 1fr; grid-auto-rows: minmax(10px, auto); } .blk { display: block; }";
    // Milestone 33 adds display none/inline/inline-block/inline-flex,
    // collapsing margins, percentages and auto margins.
    const CLASSES: &[&str] = &["", "w", "h", "p", "f", "big", "b", "bd", "c", "w p", "f bd", "n", "ib", "i", "if", "m", "e", "ib w", "m p", "mx", "g", "f g", "r", "a", "x", "s", "r a", "fx", "r p", "o", "oa", "o r", "gb", "ga", "gi", "gb ga", "fl", "fr", "cl", "fl h", "fr w", "tb", "trw", "tc", "tc w", "tb w", "gr", "gs", "ga", "gr w"];
    /// Classes that keep a box a plain block (Milestone 39c's patch oracle).
    const BLOCK_CLASSES: &[&str] = &["", "w", "p", "c", "big", "bd", "m", "e", "mx", "r", "o", "gb", "w p", "m p", "r p", "big c"];
    const WORDS: &[&str] = &["alpha", "a much longer run of words that will wrap somewhere", "x", "two words"];

    fn random_element(rng: &mut Rng, depth: usize) -> Element {
        let tag = ["div", "span", "p", "section"][rng.below(4)];
        let mut el = Element::new(tag);
        let class = CLASSES[rng.below(CLASSES.len())];
        if !class.is_empty() {
            el.set_attr("class", class);
        }
        if depth < 4 {
            for _ in 0..rng.below(4) {
                if rng.below(3) == 0 {
                    el.children.push(Node::Text(WORDS[rng.below(WORDS.len())].into()));
                } else {
                    el.append_child(random_element(rng, depth + 1));
                }
            }
        }
        el
    }

    fn all_ids(doc: &Document) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![doc.root()];
        while let Some(el) = stack.pop() {
            out.push(el.node_id());
            stack.extend(element_children(el));
        }
        out
    }

    fn mutate(rng: &mut Rng, doc: &mut Document) {
        let ids = all_ids(doc);
        let target = ids[rng.below(ids.len())];
        let other = ids[rng.below(ids.len())];
        let a_child = |doc: &Document, of: NodeId, rng: &mut Rng| -> Option<NodeId> {
            let kids: Vec<NodeId> = doc.find_by_node_id(of)?.element_children().map(|e| e.node_id()).collect();
            (!kids.is_empty()).then(|| kids[rng.below(kids.len())])
        };
        // Milestone 39: the full mutation API (ADR 021 §7).
        match rng.below(10) {
            5 => {
                let reference = a_child(doc, target, rng);
                let _ = doc.insert_before(target, random_element(rng, 2).into(), reference);
            }
            6 => {
                if let Some(child) = a_child(doc, target, rng) {
                    let _ = doc.remove_child(target, child);
                }
            }
            7 => {
                if let Some(child) = a_child(doc, target, rng) {
                    let _ = doc.replace_child(target, random_element(rng, 2).into(), child);
                }
            }
            8 => {
                let reference = a_child(doc, target, rng);
                let _ = doc.move_before(target, other, reference);
            }
            9 => {
                let word = WORDS[rng.below(WORDS.len())];
                let _ = doc.insert_before(target, dom::Fragment::text(word), a_child(doc, target, rng));
            }
            0 => {
                doc.set_attribute(target, "class", CLASSES[rng.below(CLASSES.len())]);
            }
            1 => {
                let child = random_element(rng, 3);
                let _ = doc.append_child(target, child);
            }
            2 if target != doc.root().node_id() => {
                doc.set_text_content(target, WORDS[rng.below(WORDS.len())]);
            }
            3 => {
                doc.set_inline_style_property(target, "height", &format!("{}px", rng.below(60)));
            }
            _ => {
                doc.set_inline_style_property(target, "width", &format!("{}px", 20 + rng.below(300)));
            }
        }
    }

    /// Structural equality with a float tolerance: a reused box was moved by
    /// adding a delta, a fresh one computed directly — equal up to rounding.
    /// Milestone 39b: compared in document space (a reused subtree's frame
    /// is shifted), with each subtree's bounds, order and box count, and
    /// sticky constraints.
    fn assert_same(a: &LayoutBox, b: &LayoutBox, context: &str) {
        assert_same_placed(crate::Placed::root(a), crate::Placed::root(b), context);
    }

    fn assert_same_placed(pa: crate::Placed<'_>, pb: crate::Placed<'_>, context: &str) {
        let (a, b) = (pa.layout, pb.layout);
        let close = |x: f32, y: f32| x == y || (x - y).abs() <= 1e-3 * (1.0 + x.abs().max(y.abs()));
        assert!(
            close(pa.x(), pb.x()) && close(pa.y(), pb.y()) && close(a.width, b.width) && close(a.height, b.height),
            "box geometry differs ({context}): {:?} vs {:?}",
            (pa.x(), pa.y(), a.width, a.height, a.element_key),
            (pb.x(), pb.y(), b.width, b.height, b.element_key)
        );
        assert_eq!(a.element_key, b.element_key, "{context}");
        assert_eq!(a.background_color, b.background_color, "{context}");
        assert_eq!(a.border, b.border, "{context}");
        match (&a.text, &b.text) {
            (Some(x), Some(y)) => assert!(x.content == y.content && close(pa.origin.1 + x.baseline_y, pb.origin.1 + y.baseline_y) && x.font_size == y.font_size, "{context}"),
            (None, None) => {}
            _ => panic!("text presence differs ({context})"),
        }
        // Milestone 39c: a patch leaves an ancestor's bounds a superset of
        // the exact ones, and may mark ordered children unordered (ADR 022).
        let (da, db) = (pa.to_document_space(), pb.to_document_space());
        let le = |x: f32, y: f32| x <= y || close(x, y);
        match (da.subtree.bounds, db.subtree.bounds) {
            (Some(x), Some(y)) => assert!(
                le(x.0, y.0) && le(x.1, y.1) && le(y.2, x.2) && le(y.3, x.3),
                "subtree bounds do not cover the exact ones ({context}) at {:?}: {x:?} vs {y:?}",
                a.element_key
            ),
            (x, y) => panic!("subtree bounds known in one tree only ({context}): {x:?} vs {y:?}"),
        }
        assert_eq!(a.subtree.boxes, b.subtree.boxes, "subtree box count ({context}) at {:?}", a.element_key);
        // Children marked ordered must be, by their recorded bounds — what
        // hit testing searches (grown bounds may order differently from the
        // exact ones).
        if a.subtree.ordered {
            let edges: Vec<(f32, f32)> = a.children.iter().map(|c| c.subtree.bounds.map_or((f32::NAN, f32::NAN), |(_, y0, _, y1)| (y0, y1))).collect();
            assert!(
                edges.windows(2).all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1) && !a.children.iter().any(LayoutBox::is_positioned),
                "children marked ordered when their recorded bounds are not ({context}) at {:?}",
                a.element_key
            );
        }
        let sticky = |l: &LayoutBox| l.extras().positioning.sticky.as_deref().map(|s| (s.room_above, s.room_below));
        match (sticky(a), sticky(b)) {
            (Some(x), Some(y)) => assert!(close(x.0, y.0) && close(x.1, y.1), "sticky constraints ({context}): {x:?} vs {y:?}"),
            (None, None) => {}
            _ => panic!("sticky presence differs ({context})"),
        }
        assert_eq!(a.children.len(), b.children.len(), "child count differs ({context}) under {:?}", a.element_key);
        for (ca, cb) in pa.children().zip(pb.children()) {
            assert_same_placed(ca, cb, context);
        }
    }

    /// Milestone 31's layout merge gate: incremental layout (reusing
    /// unchanged subtrees) equals a fresh layout after every step of
    /// random mutation sequences, and actually reuses most of the tree.
    #[test]
    fn incremental_layout_equals_fresh_layout_over_random_mutation_sequences() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(SHEET);
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let (mut reused, mut laid_out, mut patches) = (0usize, 0usize, 0usize);
        for sequence in 0..2_500 {
            let mut doc = Document::new(random_element(&mut rng, 0));
            let mut styles = style::StyleEngine::new();
            let mut reuse = LayoutReuse::new();
            let mut images = images::ImageCache::new();
            let width = [300.0, 640.0][rng.below(2)];
            styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(width, 600.0), &Mutations::default());
            reuse.begin_frame(None, dom::NodeIdSet::default());
            let mut previous = layout_document_incremental(doc.root(), width, &font, styles.styles(), &mut images, &mut reuse, &[]);
            for step in 0..4 {
                for _ in 0..=rng.below(2) {
                    mutate(&mut rng, &mut doc);
                }
                let mutations = doc.take_mutations();
                let report = styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(width, 600.0), &mutations);
                let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
                reuse.begin_frame(Some(previous), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
                let incremental = layout_document_incremental(doc.root(), width, &font, styles.styles(), &mut images, &mut reuse, &touched);
                reused += reuse.reused;
                laid_out += reuse.laid_out;
                let fresh = layout_document_with_style_cache(doc.root(), width, &font, styles.styles(), &mut images);
                let patched = reuse.patched().map(|(n, _)| n);
                patches += usize::from(patched.is_some());
                assert_same(&incremental, &fresh, &format!("sequence {sequence}, step {step}, patched {patched:?}"));
                previous = incremental;
            }
        }
        // The oracle is only meaningful if the reuse path really ran: these
        // small random trees are mutated near the root often, but thousands
        // of boxes must still have been moved rather than laid out.
        assert!(reused > 10_000, "the oracle exercised reuse: {reused} boxes reused ({laid_out} elements laid out)");
        assert!(patches > 0, "and patching in place (Milestone 39c): {patches} patches");
    }

    /// Milestone 39c's merge gate: on pages whose elements keep their size
    /// through most changes (fixed heights; text and class changes), so that
    /// most frames are patched in place, incremental layout still equals a
    /// fresh layout after every step — with the general mutations mixed in.
    #[test]
    fn patched_layouts_equal_fresh_layouts_over_random_mutation_sequences() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(SHEET);
        let mut rng = Rng(0x2F0C_95B3_8D1A_47E9);
        let (mut patches, mut frames) = (0usize, 0usize);
        for sequence in 0..1_500 {
            let mut doc = Document::new(random_element(&mut rng, 0));
            for id in all_ids(&doc) {
                let class = if rng.below(10) < 8 {
                    format!("{} blk", BLOCK_CLASSES[rng.below(BLOCK_CLASSES.len())])
                } else {
                    doc.find_by_node_id(id).and_then(|e| e.attr_atom(dom::atoms::CLASS)).unwrap_or("").to_string()
                };
                doc.set_attribute(id, "class", format!("{class} h"));
            }
            let _ = doc.take_mutations();
            let mut styles = style::StyleEngine::new();
            let mut reuse = LayoutReuse::new();
            let mut images = images::ImageCache::new();
            let width = [300.0, 640.0][rng.below(2)];
            styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(width, 600.0), &Mutations::default());
            reuse.begin_frame(None, dom::NodeIdSet::default());
            let mut previous = layout_document_incremental(doc.root(), width, &font, styles.styles(), &mut images, &mut reuse, &[]);
            for step in 0..5 {
                let ids = all_ids(&doc);
                let target = ids[rng.below(ids.len())];
                match rng.below(10) {
                    0..=3 => {
                        doc.set_text_content(target, WORDS[rng.below(WORDS.len())]);
                    }
                    4..=6 => {
                        doc.set_attribute(target, "class", format!("{} blk h", BLOCK_CLASSES[rng.below(BLOCK_CLASSES.len())]));
                    }
                    7 => {
                        doc.set_attribute(target, "class", format!("{} h", CLASSES[rng.below(CLASSES.len())]));
                    }
                    _ => mutate(&mut rng, &mut doc),
                }
                let mutations = doc.take_mutations();
                let report = styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(width, 600.0), &mutations);
                let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
                reuse.begin_frame(Some(previous), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
                let incremental = layout_document_incremental(doc.root(), width, &font, styles.styles(), &mut images, &mut reuse, &touched);
                let patched = reuse.patched().map(|(n, _)| n);
                patches += usize::from(patched.is_some());
                frames += 1;
                let fresh = layout_document_with_style_cache(doc.root(), width, &font, styles.styles(), &mut images);
                assert_same(&incremental, &fresh, &format!("sequence {sequence}, step {step}, patched {patched:?}"));
                previous = incremental;
            }
        }
        // Flex, inline-block, table and float ancestors (a fifth of these
        // pages' boxes) rightly rule a patch out; hundreds remain.
        assert!(patches > 600, "patched frames: {patches} of {frames}");
    }

    #[test]
    fn a_change_deep_in_a_page_relays_out_one_ancestor_chain() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(".row { height: 20px; }");
        let mut body = Element::new("body");
        for i in 0..2_000 {
            let mut row = Element::new("div");
            row.set_attr("class", "row");
            row.children.push(Node::Text(format!("row {i}")));
            body.append_child(row);
        }
        let mut doc = Document::new(body);
        let mut styles = style::StyleEngine::new();
        let mut reuse = LayoutReuse::new();
        let mut images = images::ImageCache::new();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &Mutations::default());
        reuse.begin_frame(None, dom::NodeIdSet::default());
        let first = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
        assert_eq!(reuse.laid_out, 2_001);

        let target = element_children(doc.root()).nth(1000).unwrap().node_id();
        doc.set_text_content(target, "changed text");
        let mutations = doc.take_mutations();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &mutations);
        let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).collect();
        reuse.begin_frame(Some(first), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
        let second = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
        // Milestone 39c: the row keeps its height, so it is patched in place —
        // the body is not laid out again, and no other row is visited.
        assert_eq!(reuse.laid_out, 1, "the changed row alone");
        assert_eq!(reuse.patched().map(|(n, _)| n), Some(target));
        assert_eq!(reuse.moved_box_updates, 0, "nothing moved");
        assert!(reuse.reused >= 1_999 * 2, "every other row (and its text) was kept: {}", reuse.reused);
        let fresh = layout_document_with_style_cache(doc.root(), 800.0, &font, styles.styles(), &mut images);
        assert_same(&second, &fresh, "deep change");
    }

    /// The tree with every subtree's bounds forgotten: hit testing it
    /// prunes nothing — the reference the pruned hit test must equal.
    fn without_bounds(b: &LayoutBox) -> LayoutBox {
        let mut copy = b.clone();
        let mut stack = vec![&mut copy];
        while let Some(node) = stack.pop() {
            node.subtree.bounds = None;
            node.subtree.ordered = false;
            stack.extend(node.children.iter_mut());
        }
        copy
    }

    /// Milestone 39b: hit testing that prunes by recorded bounds (and
    /// binary-searches ordered children) finds exactly what visiting every
    /// box finds — over random pages, incremental layouts, page and
    /// container scrolls.
    #[test]
    fn pruned_hit_testing_equals_visiting_every_box() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(SHEET);
        let mut rng = Rng(0x5DEE_CE66_D1CE_4E5B);
        let mut pruned_total = (0usize, 0usize);
        for sequence in 0..600 {
            let mut doc = Document::new(random_element(&mut rng, 0));
            let mut styles = style::StyleEngine::new();
            let mut reuse = LayoutReuse::new();
            let mut images = images::ImageCache::new();
            styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(640.0, 600.0), &Mutations::default());
            reuse.begin_frame(None, dom::NodeIdSet::default());
            let mut laid = layout_document_incremental(doc.root(), 640.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
            for step in 0..3 {
                let reference = without_bounds(&laid);
                let ids = all_ids(&doc);
                let scrolls: crate::ScrollOffsets = (0..rng.below(3)).map(|_| (ids[rng.below(ids.len())], (rng.below(30) as f32, rng.below(30) as f32))).collect();
                for _ in 0..25 {
                    let (x, y, scroll) = (rng.below(660) as f32 - 10.0, rng.below(700) as f32 - 10.0, [0.0, 37.0][rng.below(2)]);
                    let hit = crate::hit_test_counted(&laid, x, y, scroll, &scrolls);
                    let all = crate::hit_test_counted(&reference, x, y, scroll, &scrolls);
                    assert_eq!(hit.path, all.path, "sequence {sequence}, step {step}, point ({x}, {y}), scroll {scroll}");
                    for (a, b) in hit.boxes.iter().zip(&all.boxes) {
                        assert_eq!((a.x(), a.y(), a.layout.element_key), (b.x(), b.y(), b.layout.element_key), "the boxes hit, placed");
                    }
                    pruned_total.0 += hit.examined;
                    pruned_total.1 += all.examined;
                }
                mutate(&mut rng, &mut doc);
                let mutations = doc.take_mutations();
                let report = styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(640.0, 600.0), &mutations);
                let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
                reuse.begin_frame(Some(laid), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
                laid = layout_document_incremental(doc.root(), 640.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
            }
        }
        assert!(pruned_total.0 * 2 < pruned_total.1, "pruning examined {} boxes, visiting every box {}", pruned_total.0, pruned_total.1);
    }

    /// Milestone 39b's acceptance criterion: a click on a 10,000-row page
    /// examines O(depth) boxes, not the page — and gives the boxes it hit,
    /// so nothing searches the tree for them.
    #[test]
    fn a_click_on_a_ten_thousand_row_page_examines_a_handful_of_boxes() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(".row { padding: 2px; } span { color: #334455; }");
        let mut body = Element::new("body");
        for i in 0..10_000 {
            let mut row = Element::new("div");
            row.set_attr("class", "row");
            let mut span = Element::new("span");
            span.children.push(Node::Text(format!("row {i}")));
            row.append_child(span);
            row.children.push(Node::Text(" text".into()));
            body.append_child(row);
        }
        let mut doc = Document::new(body);
        let mut styles = style::StyleEngine::new();
        let mut reuse = LayoutReuse::new();
        let mut images = images::ImageCache::new();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &Mutations::default());
        reuse.begin_frame(None, dom::NodeIdSet::default());
        let first = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
        // A change moves every later row over (shifted frames), then clicks.
        let target = element_children(doc.root()).nth(10).unwrap().node_id();
        doc.set_text_content(target, "a longer row that wraps onto a second line, a longer row that wraps onto a second line, a longer row that wraps");
        let mutations = doc.take_mutations();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &mutations);
        let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).collect();
        reuse.begin_frame(Some(first), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
        let laid = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
        let row = element_children(doc.root()).nth(7_000).unwrap().node_id();
        let row_box = reuse.box_of(&laid, row).expect("the row's box, by its recorded place");
        let (x, y) = (row_box.x() + 3.0, row_box.y() + 5.0);
        let hit = crate::hit_test_counted(&laid, x, y, 0.0, &crate::ScrollOffsets::new());
        assert_eq!(hit.path.get(1).copied(), Some(row), "the row, under its span: {:?}", hit.path);
        assert!(hit.examined < 40, "a click examined {} boxes", hit.examined);
        assert_eq!(hit.boxes[1].layout.element_key, Some(row));
        assert_eq!((hit.boxes[1].x(), hit.boxes[1].y()), (row_box.x(), row_box.y()));
        assert_eq!(hit.path, crate::hit_test_path(&without_bounds(&laid), x, y), "the same as visiting every box");
    }

    /// Milestone 39b's acceptance criterion: moving a reused subtree costs
    /// O(1) box updates, however big the subtree — and keeps costing O(1)
    /// frame after frame, as moved subtrees move again.
    #[test]
    fn moving_a_reused_subtree_costs_one_box_update_whatever_its_size() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(".cell { padding: 1px; }");
        let mut body = Element::new("body");
        let (sections, cells) = (50, 40);
        for s in 0..sections {
            let mut section = Element::new("div");
            for c in 0..cells {
                let mut cell = Element::new("div");
                cell.set_attr("class", "cell");
                cell.children.push(Node::Text(format!("cell {s}.{c} text")));
                section.append_child(cell);
            }
            body.append_child(section);
        }
        let mut doc = Document::new(body);
        let mut styles = style::StyleEngine::new();
        let mut reuse = LayoutReuse::new();
        let mut images = images::ImageCache::new();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &Mutations::default());
        reuse.begin_frame(None, dom::NodeIdSet::default());
        let mut previous = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
        let first_section = element_children(doc.root()).next().unwrap().node_id();
        for frame in 0..3 {
            // A new first cell in the first section pushes every later
            // section down.
            let mut extra = Element::new("div");
            extra.children.push(Node::Text(format!("inserted {frame}")));
            let first_cell = element_children(doc.find_by_node_id(first_section).unwrap()).next().map(|e| e.node_id());
            doc.insert_before(first_section, dom::Fragment::Element(extra), first_cell).unwrap();
            let mutations = doc.take_mutations();
            let report = styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &mutations);
            let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
            reuse.begin_frame(Some(previous), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
            let next = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
            // The 49 later sections moved, and the first section's 40 old
            // cells: one update each, though they hold over 8,000 boxes.
            assert!(reuse.reused > 8_000, "frame {frame}: {} boxes moved over", reuse.reused);
            assert_eq!(reuse.moved_box_updates, (sections - 1) + cells + frame, "frame {frame}: one box update per moved subtree");
            let fresh = layout_document_with_style_cache(doc.root(), 800.0, &font, styles.styles(), &mut images);
            assert_same(&next, &fresh, &format!("frame {frame}"));
            previous = next;
        }
    }

    /// Performance audit: intrinsic sizes are kept across frames, so a
    /// change on a page that needs them (any flex container) re-measures
    /// the changed element and its ancestors — not the whole page, as every
    /// layout frame used to.
    #[test]
    fn a_change_on_a_flex_page_re_measures_only_its_ancestor_chain() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(".bar { display: flex; } .row { height: 20px; }");
        let mut body = Element::new("body");
        let mut bar = Element::new("div");
        bar.set_attr("class", "bar");
        bar.append_child(Element::new("span"));
        body.append_child(bar);
        for i in 0..2_000 {
            let mut row = Element::new("div");
            row.set_attr("class", "row");
            row.children.push(Node::Text(format!("row {i} with some words")));
            body.append_child(row);
        }
        let mut doc = Document::new(body);
        let mut styles = style::StyleEngine::new();
        let mut reuse = LayoutReuse::new();
        let mut images = images::ImageCache::new();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &Mutations::default());
        reuse.begin_frame(None, dom::NodeIdSet::default());
        let first = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
        assert!(reuse.intrinsic_measured > 2_000, "the first frame measures the page: {}", reuse.intrinsic_measured);

        let target = element_children(doc.root()).nth(1500).unwrap().node_id();
        doc.set_text_content(target, "changed text");
        let mutations = doc.take_mutations();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &mutations);
        let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).collect();
        reuse.begin_frame(Some(first), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
        let second = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
        assert!(reuse.intrinsic_measured <= 2, "the body and the changed row, not the page: {}", reuse.intrinsic_measured);
        let fresh = layout_document_with_style_cache(doc.root(), 800.0, &font, styles.styles(), &mut images);
        assert_same(&second, &fresh, "change on a flex page");
    }

    /// Milestone 39c: on a page that needs intrinsic sizing, a change inside
    /// one element that still contributes to its parent what it did — here
    /// a fixed-width row, whatever its text — measures that element alone:
    /// its ancestors' sizes cannot have changed.
    #[test]
    fn a_change_that_keeps_its_contribution_measures_only_its_element() {
        let font = text::Font::dejavu_sans();
        let sheet = css_parser::parse_stylesheet(".bar { display: flex; } .row { height: 20px; width: 300px; }");
        let mut body = Element::new("body");
        let mut bar = Element::new("div");
        bar.set_attr("class", "bar");
        bar.append_child(Element::new("span"));
        body.append_child(bar);
        for i in 0..2_000 {
            let mut row = Element::new("div");
            row.set_attr("class", "row");
            row.children.push(Node::Text(format!("row {i}")));
            body.append_child(row);
        }
        let mut doc = Document::new(body);
        let mut styles = style::StyleEngine::new();
        let mut reuse = LayoutReuse::new();
        let mut images = images::ImageCache::new();
        styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &Mutations::default());
        reuse.begin_frame(None, dom::NodeIdSet::default());
        let mut laid = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &[]);
        let target = element_children(doc.root()).nth(1500).unwrap().node_id();
        for text in ["a much longer text than the row had before", "x"] {
            doc.set_text_content(target, text);
            let mutations = doc.take_mutations();
            styles.restyle(&doc, &sheet, 1, style::ViewportSize::new(800.0, 600.0), &mutations);
            let touched: Vec<NodeId> = mutations.records.iter().map(|r| r.target).collect();
            reuse.begin_frame(Some(laid), LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
            laid = layout_document_incremental(doc.root(), 800.0, &font, styles.styles(), &mut images, &mut reuse, &touched);
            assert_eq!(reuse.intrinsic_measured, 1, "the row alone");
            assert_eq!(reuse.patched().map(|(n, _)| n), Some(target), "and laid out alone");
            let fresh = layout_document_with_style_cache(doc.root(), 800.0, &font, styles.styles(), &mut images);
            assert_same(&laid, &fresh, text);
        }
    }
}
