//! Milestone 39c: a change that keeps its element's size, patched into the
//! previous frame's layout in place (`docs/adr/022-layout-coordinates.md`
//! §8).
//!
//! Incremental layout (Milestone 31) lays out the changed elements' ancestor
//! chain again and moves every other subtree over — so a change to one row
//! of a 10,000-row list visits the list's 10,000 children, if only to move
//! each in O(1). When the changed element C comes out of its new layout
//! exactly as big as before, nothing after it moves and no ancestor changes
//! size: its new box is put where the old one was, and the ancestors keep
//! theirs. That is only sound when each ancestor's layout depends on C
//! through C's size alone — what the checks below establish — and when no
//! box outside C is placed from where something inside C is.

use crate::box_model::{display_of, is_float, is_inline_level, is_out_of_flow, layout_element, margin_of, style_of};
use crate::reuse::{finish_subtrees, union};
use crate::{IntrinsicCache, LayoutBox, LayoutReuse};
use dom::{ElementRef, NodeId};
use images::ImageCache;
use style::StyleCache;
use text::Font;

/// This frame's layout, if the change could be patched into the previous
/// one; `None` when the frame must lay out as usual (from a previous tree
/// that may have lost the parts of the changed element already taken —
/// they are laid out fresh).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_patch(
    root: ElementRef<'_>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut LayoutReuse,
    changed: &[NodeId],
) -> Option<LayoutBox> {
    if !reuse.has_previous() || changed.is_empty() {
        return None;
    }
    let document = root.document()?;
    let (target, elements) = changed_root(root, changed)?;
    if !elements.iter().all(|&el| laid_out_within(el, target, style_cache)) {
        return None;
    }
    // An out-of-flow box in the target whose containing block is outside it
    // is placed by that block's layout, from where its in-flow container is
    // — which may move within the target. (Only searched for on a page that
    // has had one.)
    if reuse.saw_out_of_flow && !no_box_placed_outside(target, style_cache) {
        return None;
    }
    // The target is still a block its parent's flow places, and neither it
    // nor any ancestor sizes itself from what it contains.
    if !in_flow_block(target, style_cache) || style_of(style_cache, target).position == style::Position::Sticky {
        return None;
    }
    let mut at = Some(target);
    while let Some(el) = at {
        if crate::intrinsic::sized_from_content(el, style_of(style_cache, el)) {
            return None;
        }
        at = el.parent();
    }
    let parent = target.parent()?;
    let (place, path) = reuse.patch_place(target.node_id())?;
    let saw_out_of_flow = reuse.saw_out_of_flow;
    let previous = reuse.previous_tree_mut()?;
    // ... nor was one before (a change can move an element's containing
    // block into the target): out-of-flow boxes come last among their
    // containing block's children.
    if saw_out_of_flow && placed_by_an_ancestor(previous, &path, target, document) {
        return None;
    }
    let old = box_at(previous, &path)?;
    // An empty box's margins collapse through it: its size is not all that
    // places what follows.
    if old.element_key != Some(target.node_id()) || old.height <= 0.0 {
        return None;
    }
    let old_geometry = (old.x, old.y, old.width, old.height);
    let fresh = layout_element(target, place.outer_x, place.outer_y, place.available_width, font, style_cache, intrinsic, image_cache, reuse, place.depth);
    let margin_bottom = margin_of(style_cache, target, place.available_width).bottom;
    if (fresh.x, fresh.y, fresh.width, fresh.height) != old_geometry
        || margin_bottom != place.margin_bottom
    {
        return None;
    }
    let frame = reuse.frame();
    replace_at(reuse.previous_tree_mut()?, &path, fresh, frame)?;
    reuse.finish_patch(target.node_id(), path, parent.node_id())
}

/// The topmost changed element, if every changed element is in its subtree
/// (generated content counting as its element), with the changed elements.
fn changed_root<'a>(root: ElementRef<'a>, changed: &[NodeId]) -> Option<(ElementRef<'a>, Vec<ElementRef<'a>>)> {
    let document = root.document()?;
    let mut elements = Vec::with_capacity(changed.len());
    for &id in changed {
        elements.push(document.find_by_node_id(style::owner_of(id))?);
    }
    let depth = |el: ElementRef<'_>| std::iter::successors(el.parent(), |p| p.parent()).count();
    let target = *elements.iter().min_by_key(|el| depth(**el))?;
    let inside = |el: ElementRef<'_>| std::iter::successors(Some(el), |e| e.parent()).any(|e| e.node_id() == target.node_id());
    elements.iter().all(|&el| inside(el)).then_some((target, elements))
}

/// Whether `el` is a block its parent's flow places — the classification
/// the flow makes, in the same order (`box_model::layout_flow_children`).
fn in_flow_block(el: ElementRef<'_>, style_cache: &StyleCache) -> bool {
    let display = display_of(style_cache, el);
    !(matches!(display, style::Display::None | style::Display::TableColumn)
        || is_out_of_flow(style_cache, el)
        || is_float(style_cache, el)
        || el.is(dom::atoms::BR)
        || is_inline_level(display))
}

/// Milestone 39c: the element whose subtree holds every changed element, if
/// it contributed to its parent's intrinsic sizes as an in-flow block last
/// frame (its parent's block flow placed it) and still is one — then
/// re-measuring its subtree tells whether any ancestor's sizes changed
/// (`intrinsic::measure_subtree`). With its depth.
pub(crate) fn measured_root<'a>(root: ElementRef<'a>, style_cache: &StyleCache, reuse: &LayoutReuse, changed: &[NodeId]) -> Option<(ElementRef<'a>, usize)> {
    if changed.is_empty() {
        return None;
    }
    let (target, _) = changed_root(root, changed)?;
    if target.parent().is_none() || !in_flow_block(target, style_cache) {
        return None;
    }
    let (place, _) = reuse.patch_place(target.node_id())?;
    Some((target, place.depth))
}

/// Whether `el` is laid out as part of `target`'s layout: it is `target` or
/// a descendant, and neither it nor an ancestor up to `target` is out of
/// flow with a containing block outside `target` (that block's layout, not
/// `target`'s, places it).
fn laid_out_within(el: ElementRef<'_>, target: ElementRef<'_>, style_cache: &StyleCache) -> bool {
    let mut needs_containing_block = false;
    let mut at = el;
    loop {
        let position = style_of(style_cache, at).position;
        if needs_containing_block && position.is_positioned() {
            needs_containing_block = false;
        }
        match position {
            style::Position::Fixed => return false,
            style::Position::Absolute => needs_containing_block = true,
            _ => {}
        }
        if at.node_id() == target.node_id() {
            return !needs_containing_block;
        }
        match at.parent() {
            Some(parent) => at = parent,
            None => return false,
        }
    }
}

/// Whether every out-of-flow element in `target`'s subtree is placed within
/// it (by `target` or a positioned descendant of it).
fn no_box_placed_outside(target: ElementRef<'_>, style_cache: &StyleCache) -> bool {
    let mut stack: Vec<ElementRef<'_>> = target.element_children().collect();
    while let Some(el) = stack.pop() {
        if style_of(style_cache, el).position.is_out_of_flow() && !laid_out_within(el, target, style_cache) {
            return false;
        }
        stack.extend(el.element_children());
    }
    true
}

/// Whether a box on the way down `path` (the target's ancestors) holds the
/// out-of-flow box of an element inside `target`, or of one removed.
fn placed_by_an_ancestor(root: &LayoutBox, path: &[u32], target: ElementRef<'_>, document: &dom::Document) -> bool {
    let mut node = root;
    for &i in path {
        let out_of_flow = node.children.iter().rev().take_while(|c| matches!(c.position_scheme(), style::Position::Absolute | style::Position::Fixed));
        for placed in out_of_flow {
            // (An element no longer in the document was removed from inside
            // the target: a mutation there is what removes elements.)
            let inside = placed.element_key.map(|k| document.find_by_node_id(style::owner_of(k))).is_some_and(|el| {
                el.is_none_or(|el| std::iter::successors(Some(el), |e| e.parent()).any(|e| e.node_id() == target.node_id()))
            });
            if inside {
                return true;
            }
        }
        match node.children.get(i as usize) {
            Some(child) => node = child,
            None => return true,
        }
    }
    false
}

/// The box at `path` (child indices from the root).
fn box_at<'t>(root: &'t LayoutBox, path: &[u32]) -> Option<&'t LayoutBox> {
    path.iter().try_fold(root, |node, &i| node.children.get(i as usize))
}

/// Puts `fresh` at `path` in place of the box there, and brings every
/// ancestor's subtree record up to date: box count exact, bounds grown to
/// cover the new box (a superset), order kept only if still true. Returns
/// the change in box count.
fn replace_at(node: &mut LayoutBox, path: &[u32], mut fresh: LayoutBox, frame: u32) -> Option<i64> {
    let (&index, rest) = path.split_first()?;
    let index = index as usize;
    let delta = if rest.is_empty() {
        let slot = node.children.get_mut(index)?;
        finish_subtrees(&mut fresh, frame);
        let delta = i64::from(fresh.subtree.boxes) - i64::from(slot.subtree.boxes);
        *slot = fresh;
        delta
    } else {
        replace_at(node.children.get_mut(index)?, rest, fresh, frame)?
    };
    node.subtree.boxes = u32::try_from(i64::from(node.subtree.boxes) + delta).unwrap_or(0);
    // Changed this frame: a display list builds it, not moves it over.
    node.subtree.frame = frame;
    grow_bounds(node, index);
    Some(delta)
}

/// `node`'s child `index` changed (its size did not): `node`'s bounds grow
/// to cover it, and its children stay marked ordered only if they still are
/// (see `finish_subtrees` for what bounds cover).
fn grow_bounds(node: &mut LayoutBox, index: usize) {
    let child = &node.children[index];
    let Some((x0, y0, x1, y1)) = child.subtree.bounds else { return };
    if node.subtree.ordered {
        let edges = |c: &LayoutBox| c.subtree.bounds.map(|(_, a, _, b)| (a, b));
        let before = index.checked_sub(1).and_then(|i| edges(&node.children[i]));
        let after = node.children.get(index + 1).and_then(edges);
        let in_order = before.is_none_or(|(a, b)| a <= y0 && b <= y1) && after.is_none_or(|(a, b)| y0 <= a && y1 <= b);
        if child.is_positioned() || !in_order {
            node.subtree.ordered = false;
        }
    }
    if node.overflow.x && node.overflow.y {
        return; // descendants are hit only inside the box itself
    }
    let (ox, oy) = node.subtree.child_offset;
    let mut reach = (x0 + ox, y0 + oy, x1 + ox, y1 + oy);
    if let Some((c0, d0, c1, d1)) = node.clip_rect() {
        reach = (reach.0.max(c0), reach.1.max(d0), reach.2.min(c1), reach.3.min(d1));
    }
    if reach.0 > reach.2 || reach.1 > reach.3 {
        return;
    }
    if let Some(bounds) = node.subtree.bounds {
        node.subtree.bounds = Some(union(Some(bounds), reach));
    }
}
