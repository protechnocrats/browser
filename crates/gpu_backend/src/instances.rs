//! A frame's instance data (Milestone 34) — pure, so it is tested without a
//! GPU.
//!
//! Built on the same walk as `rendering::plan_draw_passes` (and the CPU
//! rasteriser), so instance `n` of each family is exactly the one the pass
//! plan's batches count. Every instance carries its geometry in its own
//! (local) coordinates plus the tail the shaders share (`common.wgsl`): its
//! world transform and the clip it is drawn under.

use rendering::walk::{walk, Clip, Step};
use rendering::{Affine, DisplayItem};

/// Floats in the shared tail: transform (4 + 2), clip (4), clip extra (4),
/// local clip (4), its transform (4 + 2).
pub const TAIL: [u32; 7] = [4, 2, 4, 4, 4, 4, 2];
/// Per-family attributes before the tail, as vector widths.
pub const QUAD_HEAD: [u32; 3] = [4, 4, 2];
pub const GLYPH_HEAD: [u32; 3] = [4, 4, 4];
pub const IMAGE_HEAD: [u32; 4] = [4, 4, 2, 4];
pub const COMPOSITE: [u32; 2] = [4, 1];

pub const fn floats(head: &[u32]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < head.len() {
        n += head[i] as usize;
        i += 1;
    }
    n
}

pub const QUAD_FLOATS: usize = floats(&QUAD_HEAD) + floats(&TAIL);
pub const GLYPH_FLOATS: usize = floats(&GLYPH_HEAD) + floats(&TAIL);
pub const IMAGE_FLOATS: usize = floats(&IMAGE_HEAD) + floats(&TAIL);

/// Where an image is drawn from: the part of its texture holding it,
/// normalised (`[x0, y0, x1, y1]`). `None`: not on the GPU (refused) — its
/// instance is still built, so offsets line up, and draws nothing.
pub type ImageRect = Option<[f32; 4]>;

#[derive(Debug, Default)]
pub struct Instances {
    pub quads: Vec<f32>,
    pub glyphs: Vec<f32>,
    pub images: Vec<f32>,
}

impl Instances {
    pub fn quad_count(&self) -> u32 {
        (self.quads.len() / QUAD_FLOATS) as u32
    }

    /// Appends a plain world-space rect (the canvas colour of a damage
    /// redraw); returns its instance index.
    pub fn push_plain_rect(&mut self, rect: [f32; 4], color: [f32; 4]) -> u32 {
        let index = self.quad_count();
        self.quads.extend_from_slice(&rect);
        self.quads.extend_from_slice(&color);
        self.quads.extend_from_slice(&[0.0, 0.0]);
        push_tail(&mut self.quads, &Affine::IDENTITY, &Clip::NONE);
        index
    }
}

fn push_tail(out: &mut Vec<f32>, t: &Affine, clip: &Clip) {
    let [a, b, c, d, e, f] = t.0;
    out.extend_from_slice(&[a, b, c, d, e, f]);
    out.extend_from_slice(&clip.rect);
    match &clip.local {
        Some(local) => {
            let [la, lb, lc, ld, le, lf] = local.to_local.0;
            out.extend_from_slice(&[clip.radius, 1.0, local.radius, 0.0]);
            out.extend_from_slice(&local.rect);
            out.extend_from_slice(&[la, lb, lc, ld, le, lf]);
        }
        None => {
            out.extend_from_slice(&[clip.radius, 0.0, 0.0, 0.0]);
            out.extend_from_slice(&[0.0; 4]);
            out.extend_from_slice(&Affine::IDENTITY.0);
        }
    }
}

fn color(c: style::Color) -> [f32; 4] {
    [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0, f32::from(c.a) / 255.0]
}

/// The instances for `items`. `atlas` is the glyph atlas's size;
/// `image_rect` says where each image is.
pub fn build(items: &[DisplayItem], atlas: (f32, f32), image_rect: impl Fn(u64) -> ImageRect) -> Instances {
    let mut out = Instances::default();
    let (aw, ah) = (atlas.0.max(1.0), atlas.1.max(1.0));
    walk(items, |step| {
        let Step::Draw { item, transform, clip } = step else { return };
        match item {
            DisplayItem::Rect { x, y, width, height, color: c, radius } => {
                out.quads.extend_from_slice(&[*x, *y, *width, *height]);
                out.quads.extend_from_slice(&color(*c));
                out.quads.extend_from_slice(&[radius.max(0.0), 0.0]);
                push_tail(&mut out.quads, &transform, &clip);
            }
            DisplayItem::Shadow { x, y, width, height, color: c, radius, blur } => {
                out.quads.extend_from_slice(&[*x, *y, *width, *height]);
                out.quads.extend_from_slice(&color(*c));
                out.quads.extend_from_slice(&[radius.max(0.0), blur.max(0.0)]);
                push_tail(&mut out.quads, &transform, &clip);
            }
            DisplayItem::Glyphs { instances, .. } => {
                for g in instances {
                    out.glyphs.extend_from_slice(&[g.x, g.y, g.width, g.height]);
                    // Normalised against the atlas's CURRENT size: it may
                    // have grown since the glyph was packed.
                    out.glyphs.extend_from_slice(&[g.atlas_x as f32 / aw, g.atlas_y as f32 / ah, g.atlas_w as f32 / aw, g.atlas_h as f32 / ah]);
                    out.glyphs.extend_from_slice(&color(g.color));
                    push_tail(&mut out.glyphs, &transform, &clip);
                }
            }
            DisplayItem::Image { x, y, width, height, image_id, sampling } => {
                let rendering::ImageSampling { uv, alpha } = rendering::ImageSampling::of(sampling);
                let rect = image_rect(*image_id);
                out.images.extend_from_slice(&[*x, *y, *width, *height]);
                out.images.extend_from_slice(&uv);
                let tiled = if rendering::uv_tiles(uv) { 1.0 } else { 0.0 };
                // A refused image draws at opacity 0 (its batch is skipped
                // anyway; this keeps a stray draw harmless).
                out.images.extend_from_slice(&[tiled, if rect.is_some() { alpha.clamp(0.0, 1.0) } else { 0.0 }]);
                out.images.extend_from_slice(&rect.unwrap_or([0.0, 0.0, 1.0, 1.0]));
                push_tail(&mut out.images, &transform, &clip);
            }
            _ => {}
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rendering::display_list::GlyphInstance;
    use rendering::{BatchKind, PassOp, TransformLayer};
    use style::Color;

    const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };

    #[test]
    fn instances_line_up_with_the_pass_plan() {
        let g = GlyphInstance { x: 0.0, y: 0.0, width: 1.0, height: 1.0, atlas_x: 2, atlas_y: 4, atlas_w: 1, atlas_h: 1, color: RED };
        let items = vec![
            DisplayItem::Rect { x: 0.0, y: 0.0, width: 5.0, height: 5.0, color: RED, radius: 0.0 },
            DisplayItem::PushEffect { opacity: 0.5, bounds: [0.0, 0.0, 10.0, 10.0] },
            DisplayItem::Glyphs { instances: vec![g; 3], atlas_generation: 0 },
            DisplayItem::Image { x: 0.0, y: 0.0, width: 2.0, height: 2.0, image_id: 7, sampling: None },
            DisplayItem::PopEffect,
            DisplayItem::Shadow { x: 0.0, y: 0.0, width: 5.0, height: 5.0, color: RED, radius: 1.0, blur: 3.0 },
        ];
        let inst = build(&items, (8.0, 8.0), |_| Some([0.0, 0.0, 0.5, 0.5]));
        assert_eq!(inst.quads.len(), 2 * QUAD_FLOATS);
        assert_eq!(inst.glyphs.len(), 3 * GLYPH_FLOATS);
        assert_eq!(inst.images.len(), IMAGE_FLOATS);
        // Every batch's range is inside its family's instances.
        for op in rendering::plan_draw_passes(&items, |id| id) {
            if let PassOp::Draw(b) = op {
                let n = match b.kind {
                    BatchKind::Rects => inst.quads.len() / QUAD_FLOATS,
                    BatchKind::Glyphs => inst.glyphs.len() / GLYPH_FLOATS,
                    BatchKind::Image(_) => inst.images.len() / IMAGE_FLOATS,
                };
                assert!((b.first + b.count) as usize <= n, "{b:?}");
            }
        }
        assert_eq!(&inst.glyphs[4..8], &[0.25, 0.5, 0.125, 0.125], "atlas coordinates normalised");
        assert_eq!(inst.quads[QUAD_FLOATS + 9], 3.0, "the shadow's blur");
    }

    #[test]
    fn the_tail_carries_the_transform_and_a_local_clip() {
        let rot = Affine::rotate_deg(30.0);
        let items = vec![
            DisplayItem::PushTransform(Box::new(TransformLayer { matrix: rot, bounds: [0.0; 4], element: None, origin: [0.0; 2], scroll: None })),
            DisplayItem::PushClip { rect: [0.0, 0.0, 4.0, 4.0], radius: 1.0 },
            DisplayItem::Rect { x: 0.0, y: 0.0, width: 5.0, height: 5.0, color: RED, radius: 0.0 },
        ];
        let inst = build(&items, (1.0, 1.0), |_| None);
        let tail = &inst.quads[floats(&QUAD_HEAD)..];
        assert_eq!(&tail[..6], &rot.0);
        assert_eq!(tail[10 + 1], 1.0, "a clip under rotation is applied in its own space");
        assert_eq!(&tail[14..18], &[0.0, 0.0, 4.0, 4.0]);
        assert_eq!(tail.len(), floats(&TAIL));
    }

    #[test]
    fn a_refused_image_still_takes_its_slot() {
        let items = vec![DisplayItem::Image { x: 0.0, y: 0.0, width: 2.0, height: 2.0, image_id: 1, sampling: None }];
        let inst = build(&items, (1.0, 1.0), |_| None);
        assert_eq!(inst.images.len(), IMAGE_FLOATS);
        assert_eq!(inst.images[9], 0.0, "drawn at opacity 0");
    }
}
