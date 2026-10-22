//! Reading a display list's structure (Milestone 34): the one walk every
//! renderer uses — the GPU backend and the CPU rasteriser — so they cannot
//! disagree about what a clip, transform or group applies to.
//!
//! [`walk`] turns the `Push*`/`Pop*` markers into, for each drawing item,
//! its world transform and the clip it is drawn under, and into group
//! (effect) begin/end steps. It tolerates a malformed list — a pop with no
//! push is ignored, pushes left open are closed at the end — because a
//! scene can come from a renderer process the browser does not trust.

use crate::display_list::DisplayItem;
use crate::Affine;

/// Coordinates no clip reaches: "not clipped".
const OPEN: f32 = 1.0e30;

/// A clip in world space, as a renderer applies it per pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    /// `[x0, y0, x1, y1]`: the intersection of every enclosing clip whose
    /// transform keeps it axis-aligned (and the bounding box of the others).
    pub rect: [f32; 4],
    /// Corner radius of `rect` — the innermost rounded clip's, when that
    /// clip is what bounds `rect` on every side; 0 otherwise.
    pub radius: f32,
    /// The innermost clip under a rotating or skewing transform, applied
    /// exactly in its own space.
    pub local: Option<LocalClip>,
}

/// A clip in the space of a non-axis-aligned transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalClip {
    /// `[x0, y0, x1, y1]` in the clip's own space.
    pub rect: [f32; 4],
    pub radius: f32,
    /// Maps a world point into that space.
    pub to_local: Affine,
}

impl Clip {
    pub const NONE: Clip = Clip { rect: [-OPEN, -OPEN, OPEN, OPEN], radius: 0.0, local: None };

    /// Whether the clip cuts nothing (no clip at all).
    pub fn is_none(&self) -> bool {
        self.local.is_none() && self.rect[0] <= -OPEN && self.rect[1] <= -OPEN && self.rect[2] >= OPEN && self.rect[3] >= OPEN
    }

    /// Whether world point (`x`, `y`) is inside — the coverage a renderer
    /// multiplies in (a one-pixel ramp on rounded corners).
    pub fn coverage(&self, x: f32, y: f32) -> f32 {
        let mut c = rect_coverage(self.rect, self.radius, x, y);
        if let Some(local) = &self.local {
            let (lx, ly) = local.to_local.apply(x, y);
            c *= rect_coverage(local.rect, local.radius, lx, ly);
        }
        c
    }

    fn intersect_world(&self, r: [f32; 4], radius: f32) -> Clip {
        let rect = [self.rect[0].max(r[0]), self.rect[1].max(r[1]), self.rect[2].min(r[2]), self.rect[3].min(r[3])];
        // The rounded corners survive only when one clip bounds every side.
        let radius = if rect == r {
            radius
        } else if rect == self.rect {
            self.radius
        } else {
            0.0
        };
        Clip { rect, radius, local: self.local }
    }
}

fn rect_coverage(r: [f32; 4], radius: f32, x: f32, y: f32) -> f32 {
    if x < r[0] || y < r[1] || x >= r[2] || y >= r[3] {
        return 0.0;
    }
    if radius <= 0.0 {
        return 1.0;
    }
    let radius = radius.min((r[2] - r[0]) / 2.0).min((r[3] - r[1]) / 2.0);
    let nx = x.clamp(r[0] + radius, r[2] - radius);
    let ny = y.clamp(r[1] + radius, r[3] - radius);
    let d = ((x - nx).powi(2) + (y - ny).powi(2)).sqrt();
    (radius + 0.5 - d).clamp(0.0, 1.0)
}

/// One step of a walk.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    /// A drawing item, its world transform, and its clip.
    Draw { item: &'a DisplayItem, transform: Affine, clip: Clip },
    /// A group begins: what follows until its [`Step::EndGroup`] is drawn as
    /// one layer, then the layer at `opacity`. `bounds` (`[x0, y0, x1,
    /// y1]`, world, already clipped) is all a layer needs to cover.
    BeginGroup { opacity: f32, bounds: [f32; 4], clip: Clip },
    EndGroup,
}

enum Frame {
    Clip(Clip),
    Transform(Affine),
    Group,
}

/// Walks `items` in paint order, calling `f` for every drawing item and
/// group boundary.
pub fn walk<'a>(items: &'a [DisplayItem], mut f: impl FnMut(Step<'a>)) {
    let mut stack: Vec<Frame> = Vec::new();
    let mut transform = Affine::IDENTITY;
    let mut clip = Clip::NONE;
    // Rebuilds the current transform and clip from the stack (pops are
    // rare next to draws; walking the stack keeps this simple and exact).
    fn current(stack: &[Frame]) -> (Affine, Clip) {
        let (mut t, mut c) = (Affine::IDENTITY, Clip::NONE);
        for frame in stack {
            match frame {
                Frame::Transform(m) => t = t.then_after(m),
                Frame::Clip(cl) => c = *cl,
                Frame::Group => {}
            }
        }
        (t, c)
    }
    for item in items {
        match item {
            DisplayItem::PushClip { rect, radius } => {
                let local = [rect[0], rect[1], rect[0] + rect[2].max(0.0), rect[1] + rect[3].max(0.0)];
                let new = if transform.is_axis_aligned() {
                    let (x, y, w, h) = transform.map_rect((local[0], local[1], local[2] - local[0], local[3] - local[1]));
                    let scale = (transform.0[0] * transform.0[3]).abs().sqrt();
                    clip.intersect_world([x, y, x + w, y + h], radius.max(0.0) * scale)
                } else {
                    // Exactly in its own space; its bounding box narrows the
                    // world rectangle too (never cutting more than it).
                    let (x, y, w, h) = transform.map_rect((local[0], local[1], local[2] - local[0], local[3] - local[1]));
                    let mut new = clip.intersect_world([x, y, x + w, y + h], 0.0);
                    new.radius = if new.rect == clip.rect { clip.radius } else { 0.0 };
                    if let Some(to_local) = transform.inverse() {
                        new.local = Some(LocalClip { rect: local, radius: radius.max(0.0), to_local });
                    }
                    new
                };
                stack.push(Frame::Clip(new));
                clip = new;
            }
            DisplayItem::PushTransform(layer) => {
                let m = if layer.matrix.is_finite() { layer.matrix } else { Affine::IDENTITY };
                stack.push(Frame::Transform(m));
                transform = transform.then_after(&m);
            }
            DisplayItem::PushEffect { opacity, bounds } => {
                let (x, y, w, h) = transform.map_rect((bounds[0], bounds[1], bounds[2], bounds[3]));
                let b = [x.max(clip.rect[0]), y.max(clip.rect[1]), (x + w).min(clip.rect[2]), (y + h).min(clip.rect[3])];
                stack.push(Frame::Group);
                f(Step::BeginGroup { opacity: opacity.clamp(0.0, 1.0), bounds: b, clip });
            }
            DisplayItem::PopClip | DisplayItem::PopTransform | DisplayItem::PopEffect => {
                // Close the innermost frame of this kind; a pop with nothing
                // to close is ignored.
                let want = |fr: &Frame| {
                    matches!(
                        (item, fr),
                        (DisplayItem::PopClip, Frame::Clip(_)) | (DisplayItem::PopTransform, Frame::Transform(_)) | (DisplayItem::PopEffect, Frame::Group)
                    )
                };
                if matches!(stack.last(), Some(fr) if want(fr)) {
                    if matches!(stack.pop(), Some(Frame::Group)) {
                        f(Step::EndGroup);
                    }
                    (transform, clip) = current(&stack);
                }
            }
            _ => f(Step::Draw { item, transform, clip }),
        }
    }
    for frame in stack.iter().rev() {
        if matches!(frame, Frame::Group) {
            f(Step::EndGroup);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::TransformLayer;
    use style::Color;

    fn rect() -> DisplayItem {
        DisplayItem::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0, color: Color { r: 0, g: 0, b: 0, a: 255 }, radius: 0.0 }
    }

    fn states(items: &[DisplayItem]) -> Vec<String> {
        let mut out = Vec::new();
        walk(items, |s| {
            out.push(match s {
                Step::Draw { transform, clip, .. } => format!("draw t={:?} clip={:?}/{}", transform.0, clip.rect, clip.local.is_some()),
                Step::BeginGroup { opacity, .. } => format!("begin {opacity}"),
                Step::EndGroup => "end".into(),
            })
        });
        out
    }

    #[test]
    fn clips_intersect_transforms_compose_and_groups_nest() {
        let t = |m: Affine| DisplayItem::PushTransform(Box::new(TransformLayer { matrix: m, bounds: [0.0; 4], element: None, origin: [0.0; 2], scroll: None }));
        let items = vec![
            DisplayItem::PushClip { rect: [0.0, 0.0, 100.0, 100.0], radius: 0.0 },
            t(Affine::translate(10.0, 0.0)),
            DisplayItem::PushClip { rect: [0.0, 0.0, 50.0, 200.0], radius: 4.0 },
            rect(),
            DisplayItem::PushEffect { opacity: 0.5, bounds: [0.0, 0.0, 10.0, 10.0] },
            rect(),
            DisplayItem::PopEffect,
            DisplayItem::PopClip,
            DisplayItem::PopTransform,
            rect(),
            DisplayItem::PopClip,
            rect(),
        ];
        let got = states(&items);
        assert_eq!(got[0], format!("draw t={:?} clip={:?}/false", Affine::translate(10.0, 0.0).0, [10.0, 0.0, 60.0, 100.0]));
        assert_eq!(got[1], "begin 0.5");
        assert_eq!(got[3], "end");
        assert_eq!(got[4], format!("draw t={:?} clip={:?}/false", Affine::IDENTITY.0, [0.0, 0.0, 100.0, 100.0]));
        assert_eq!(got[5], format!("draw t={:?} clip={:?}/false", Affine::IDENTITY.0, Clip::NONE.rect));
    }

    #[test]
    fn a_clip_under_rotation_is_exact_in_its_own_space() {
        let rot = Affine::translate(50.0, 50.0).then_after(&Affine::rotate_deg(45.0)).then_after(&Affine::translate(-50.0, -50.0));
        let items = vec![
            DisplayItem::PushTransform(Box::new(TransformLayer { matrix: rot, bounds: [0.0; 4], element: None, origin: [50.0, 50.0], scroll: None })),
            DisplayItem::PushClip { rect: [40.0, 40.0, 20.0, 20.0], radius: 0.0 },
            rect(),
        ];
        let mut clip = None;
        walk(&items, |s| {
            if let Step::Draw { clip: c, .. } = s {
                clip = Some(c);
            }
        });
        let clip = clip.unwrap();
        assert_eq!(clip.coverage(50.0, 50.0), 1.0, "the centre");
        assert_eq!(clip.coverage(50.0, 36.5), 1.0, "a point inside the rotated square but outside the unrotated one");
        assert_eq!(clip.coverage(41.0, 41.0), 0.0, "an unrotated corner, outside the rotated square");
    }

    #[test]
    fn a_malformed_list_is_walked_without_panicking() {
        let items = vec![DisplayItem::PopEffect, DisplayItem::PopClip, DisplayItem::PushEffect { opacity: 2.0, bounds: [0.0; 4] }, rect()];
        let got = states(&items);
        assert_eq!(got, vec!["begin 1".to_string(), got[1].clone(), "end".to_string()], "stray pops ignored, the open group closed");
    }
}
