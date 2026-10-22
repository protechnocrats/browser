//! Turning a display list into ordered draw batches.
//!
//! The GPU backend draws three kinds of instanced primitive (solid rects,
//! glyph quads, images), each with its own pipeline. It used to draw ALL
//! rects, then ALL glyphs, then ALL images — so paint order was decided by
//! primitive *type*, not by the display list: an image was always on top of
//! every glyph, and a rect (say a later element's background) could never
//! cover earlier text. Images were also grouped through a `HashMap`, making
//! the draw order among different images nondeterministic from frame to
//! frame.
//!
//! [`plan_draw_batches`] fixes that without giving up instancing: it walks
//! the display list in order and emits maximal runs of *adjacent* items
//! that can share one draw call. Executing the batches in order paints
//! exactly what the display list says, in exactly that order, and the same
//! input always yields the same batches.
//!
//! This lives in `rendering` (platform-free) rather than `gpu_backend` so
//! the ordering logic is unit-tested without a GPU; the backend only turns
//! each batch into one `draw` over the matching range of its per-kind
//! instance buffer.

use crate::DisplayItem;

/// Which pipeline (and, for images, which texture) a batch draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchKind {
    Rects,
    Glyphs,
    /// Images in one texture — a standalone image's, or a shared image
    /// atlas page's (Milestone 34): images in the same texture batch.
    Image(u64),
}

/// One draw call: `count` instances of `kind`, starting at instance
/// `first` **within that kind's instance buffer**. The buffers are built
/// by walking the display list in order, one buffer per primitive family
/// (all images share one), so `first` is a running per-family offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawBatch {
    pub kind: BatchKind,
    pub first: u32,
    pub count: u32,
}

/// Milestone 34: what a renderer executes, in order — draw calls, and the
/// start and end of groups (each drawn into a layer of its own, then onto
/// the layer around it at `opacity`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PassOp {
    Draw(DrawBatch),
    /// `bounds` (`[x0, y0, x1, y1]`, world) is all the group's layer covers.
    BeginGroup { opacity: f32, bounds: [f32; 4] },
    EndGroup,
}

/// How many instances an item draws, and in which family.
fn family(item: &DisplayItem, texture_of: &impl Fn(u64) -> u64) -> Option<(BatchKind, u32)> {
    match item {
        // A shadow is drawn by the rect pipeline (a feathered edge).
        DisplayItem::Rect { .. } | DisplayItem::Shadow { .. } => Some((BatchKind::Rects, 1)),
        DisplayItem::Glyphs { instances, .. } => Some((BatchKind::Glyphs, instances.len() as u32)),
        DisplayItem::Image { image_id, .. } => Some((BatchKind::Image(texture_of(*image_id)), 1)),
        _ => None,
    }
}

/// How many items a merge looks past (bounded, so planning stays linear).
const MERGE_LOOKAHEAD: usize = 256;

/// Plans the passes for `items`, painting exactly what their order says:
/// runs of items that share a pipeline and texture become one draw call —
/// adjacent ones, and (Milestone 34) an item that overlaps nothing painted
/// since the last batch of its kind joins that batch, painted earlier than
/// its turn with the same result. (A gallery's thumbnails, each between
/// its background and its caption, draw as one batch of backgrounds, one
/// of images, one of captions.) A group boundary ends every run. Because a
/// family's instances are numbered in walk order, a batch stays one
/// contiguous range of its family's instances.
///
/// `texture_of` names the texture each image is drawn from. Walks the list
/// as every renderer does (`crate::walk`), so the instances a renderer
/// builds on the same walk line up with it.
pub fn plan_draw_passes(items: &[DisplayItem], texture_of: impl Fn(u64) -> u64) -> Vec<PassOp> {
    let mut ops: Vec<PassOp> = Vec::new();
    // Per op: the world rectangles its items cover (`[x0, y0, x1, y1]`).
    let mut covers: Vec<Vec<[f32; 4]>> = Vec::new();
    // Per family (rects, glyphs, images): its last batch since the last
    // group boundary.
    let mut last: [Option<usize>; 3] = [None; 3];
    // Running instance offsets, per family.
    let mut offsets = [0u32; 3];
    crate::walk::walk(items, |step| match step {
        crate::walk::Step::Draw { item, transform, clip } => {
            let Some((kind, count)) = family(item, &texture_of) else { return };
            if count == 0 {
                return;
            }
            let f = match kind {
                BatchKind::Rects => 0,
                BatchKind::Glyphs => 1,
                BatchKind::Image(_) => 2,
            };
            let first = offsets[f];
            offsets[f] += count;
            let cover = world_cover(item, &transform, &clip);
            if let Some(j) = last[f] {
                // Adjacent, or overlapping nothing painted since.
                let adjacent = j + 1 == ops.len();
                let since: usize = covers[j + 1..].iter().map(Vec::len).sum();
                let clear = adjacent
                    || (since <= MERGE_LOOKAHEAD && cover.is_some_and(|c| covers[j + 1..].iter().flatten().all(|o| !overlaps(&c, o))));
                if let PassOp::Draw(batch) = &mut ops[j] {
                    if batch.kind == kind && clear {
                        debug_assert_eq!(batch.first + batch.count, first, "a family's batches are contiguous");
                        batch.count += count;
                        covers[j].push(cover.unwrap_or(EVERYTHING));
                        return;
                    }
                }
            }
            ops.push(PassOp::Draw(DrawBatch { kind, first, count }));
            covers.push(vec![cover.unwrap_or(EVERYTHING)]);
            last[f] = Some(ops.len() - 1);
        }
        crate::walk::Step::BeginGroup { opacity, bounds, .. } => {
            ops.push(PassOp::BeginGroup { opacity, bounds });
            covers.push(Vec::new());
            last = [None; 3];
        }
        crate::walk::Step::EndGroup => {
            ops.push(PassOp::EndGroup);
            covers.push(Vec::new());
            last = [None; 3];
        }
    });
    ops
}

const EVERYTHING: [f32; 4] = [f32::NEG_INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::INFINITY];

/// What `item` can paint, in world space — its bounds through its transform,
/// cut to its clip, a pixel wider for antialiasing; `None` when unknown (it
/// then moves past nothing, and nothing moves past it).
fn world_cover(item: &DisplayItem, transform: &crate::Affine, clip: &crate::walk::Clip) -> Option<[f32; 4]> {
    let (x, y, w, h) = transform.map_rect(crate::display_list::item_bounds(item));
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return None;
    }
    let r = [(x - 1.0).max(clip.rect[0]), (y - 1.0).max(clip.rect[1]), (x + w + 1.0).min(clip.rect[2]), (y + h + 1.0).min(clip.rect[3])];
    Some(r)
}

fn overlaps(a: &[f32; 4], b: &[f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// The draw calls for a list with no groups, each image its own texture.
pub fn plan_draw_batches(items: &[DisplayItem]) -> Vec<DrawBatch> {
    plan_draw_passes(items, |id| id)
        .into_iter()
        .filter_map(|op| match op {
            PassOp::Draw(b) => Some(b),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::GlyphInstance;
    use style::Color;

    const C: Color = Color { r: 0, g: 0, b: 0, a: 255 };

    fn rect() -> DisplayItem {
        DisplayItem::Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0, color: C, radius: 0.0 }
    }
    fn image(id: u64) -> DisplayItem {
        DisplayItem::Image { x: 0.0, y: 0.0, width: 1.0, height: 1.0, image_id: id, sampling: crate::display_list::ImageSampling::boxed(crate::display_list::FULL_UV, 1.0) }
    }
    fn glyphs(n: usize) -> DisplayItem {
        let g = GlyphInstance { x: 0.0, y: 0.0, width: 1.0, height: 1.0, atlas_x: 0, atlas_y: 0, atlas_w: 1, atlas_h: 1, color: C };
        DisplayItem::Glyphs { instances: vec![g; n], atlas_generation: 0 }
    }
    fn b(kind: BatchKind, first: u32, count: u32) -> DrawBatch {
        DrawBatch { kind, first, count }
    }

    #[test]
    fn interleaved_kinds_keep_display_list_order() {
        // rect(bg) text rect(a later, overlapping bg) image image(same) image(other) rect
        let items = vec![rect(), glyphs(3), rect(), image(1), image(1), image(2), rect()];
        assert_eq!(
            plan_draw_batches(&items),
            vec![
                b(BatchKind::Rects, 0, 1),
                b(BatchKind::Glyphs, 0, 3),
                b(BatchKind::Rects, 1, 1), // drawn AFTER the text, so it can cover it
                b(BatchKind::Image(1), 0, 2),
                b(BatchKind::Image(2), 2, 1),
                b(BatchKind::Rects, 2, 1),
            ]
        );
    }

    #[test]
    fn adjacent_same_kind_items_merge_into_one_draw_call() {
        let items = vec![rect(), rect(), rect(), glyphs(2), glyphs(5)];
        assert_eq!(plan_draw_batches(&items), vec![b(BatchKind::Rects, 0, 3), b(BatchKind::Glyphs, 0, 7)]);
    }

    #[test]
    fn a_later_rect_is_never_hoisted_below_earlier_text() {
        // The old backend drew all rects first, so this rect would have
        // been painted UNDER the text it is meant to cover.
        let items = vec![glyphs(1), rect()];
        let plan = plan_draw_batches(&items);
        assert_eq!(plan[0].kind, BatchKind::Glyphs);
        assert_eq!(plan[1].kind, BatchKind::Rects);
    }

    #[test]
    fn text_after_an_image_is_drawn_after_it() {
        // The old backend drew images LAST, covering all text.
        let plan = plan_draw_batches(&[image(7), glyphs(2)]);
        assert_eq!((plan[0].kind, plan[1].kind), (BatchKind::Image(7), BatchKind::Glyphs));
    }

    #[test]
    fn different_images_are_separate_draws_in_display_list_order() {
        let plan = plan_draw_batches(&[image(9), image(3), image(9)]);
        let ids: Vec<BatchKind> = plan.iter().map(|p| p.kind).collect();
        assert_eq!(ids, vec![BatchKind::Image(9), BatchKind::Image(3), BatchKind::Image(9)], "no HashMap order, no regrouping");
    }

    #[test]
    fn items_that_draw_nothing_are_skipped_without_splitting_neighbours() {
        assert_eq!(plan_draw_batches(&[rect(), glyphs(0), rect()]), vec![b(BatchKind::Rects, 0, 2)]);
        assert!(plan_draw_batches(&[glyphs(0)]).is_empty());
        assert!(plan_draw_batches(&[]).is_empty());
    }

    #[test]
    fn the_plan_is_deterministic() {
        let items: Vec<DisplayItem> = (0..200).map(|i| match i % 5 { 0 => rect(), 1 => glyphs(i % 7), 2 => image((i % 3) as u64), 3 => rect(), _ => glyphs(2) }).collect();
        assert_eq!(plan_draw_batches(&items), plan_draw_batches(&items));
    }

    #[test]
    fn executing_the_plan_paints_every_instance_exactly_once_in_order() {
        // Property check: per-family instance ranges tile [0, total) with no
        // gap, overlap, or reordering.
        let items: Vec<DisplayItem> = (0..500).map(|i| match (i * 7 + i / 3) % 4 { 0 => rect(), 1 => glyphs(i % 5), 2 => image((i % 4) as u64), _ => rect() }).collect();
        let plan = plan_draw_batches(&items);
        let (mut next_rect, mut next_glyph, mut next_image) = (0, 0, 0);
        for batch in &plan {
            let next = match batch.kind {
                BatchKind::Rects => &mut next_rect,
                BatchKind::Glyphs => &mut next_glyph,
                BatchKind::Image(_) => &mut next_image,
            };
            assert_eq!(batch.first, *next, "ranges are contiguous and in order");
            *next += batch.count;
        }
        let total = |f: fn(&DisplayItem) -> u32| items.iter().map(f).sum::<u32>();
        assert_eq!(next_rect, total(|i| matches!(i, DisplayItem::Rect { .. }) as u32));
        assert_eq!(next_glyph, total(|i| if let DisplayItem::Glyphs { instances, .. } = i { instances.len() as u32 } else { 0 }));
        assert_eq!(next_image, total(|i| matches!(i, DisplayItem::Image { .. }) as u32));
    }

    fn rect_at(x: f32, y: f32) -> DisplayItem {
        DisplayItem::Rect { x, y, width: 10.0, height: 10.0, color: C, radius: 0.0 }
    }
    fn image_at(id: u64, x: f32, y: f32) -> DisplayItem {
        DisplayItem::Image { x, y, width: 10.0, height: 10.0, image_id: id, sampling: None }
    }

    /// Milestone 34: an item joins the last batch of its kind when it
    /// overlaps nothing painted since — and only then.
    #[test]
    fn an_item_joins_an_earlier_batch_only_past_items_it_does_not_overlap() {
        // Two cards far apart: background, image; background, image.
        let apart = [rect_at(0.0, 0.0), image_at(1, 0.0, 0.0), rect_at(100.0, 0.0), image_at(1, 100.0, 0.0)];
        let kinds: Vec<(BatchKind, u32)> = plan_draw_batches(&apart).iter().map(|b| (b.kind, b.count)).collect();
        assert_eq!(kinds, vec![(BatchKind::Rects, 2), (BatchKind::Image(1), 2)]);

        // The second background covers the first image: it must paint after
        // it (the second image, clear of it, may still join the first).
        let over = [rect_at(0.0, 0.0), image_at(1, 0.0, 0.0), rect_at(5.0, 5.0), image_at(1, 100.0, 0.0)];
        let kinds: Vec<(BatchKind, u32)> = plan_draw_batches(&over).iter().map(|b| (b.kind, b.count)).collect();
        assert_eq!(kinds, vec![(BatchKind::Rects, 1), (BatchKind::Image(1), 2), (BatchKind::Rects, 1)]);
        // ...but one under that background may not.
        let under = [rect_at(0.0, 0.0), image_at(1, 0.0, 0.0), rect_at(5.0, 5.0), image_at(1, 8.0, 8.0)];
        assert_eq!(plan_draw_batches(&under).len(), 4);

        // Never across a group: the group's layer is drawn in between.
        let grouped = [
            rect_at(0.0, 0.0),
            DisplayItem::PushEffect { opacity: 0.5, bounds: [50.0, 0.0, 10.0, 10.0] },
            rect_at(50.0, 0.0),
            DisplayItem::PopEffect,
            rect_at(100.0, 0.0),
        ];
        assert_eq!(plan_draw_passes(&grouped, |id| id).iter().filter(|op| matches!(op, PassOp::Draw(_))).count(), 3);
    }
}
