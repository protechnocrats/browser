//! 2D affine transforms (Milestone 34): what `PushTransform` carries, and
//! how every consumer of a display list maps a point through it.

/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f` — the CSS `matrix(a, b, c, d,
/// e, f)` order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine(pub [f32; 6]);

impl Affine {
    pub const IDENTITY: Affine = Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub fn translate(x: f32, y: f32) -> Affine {
        Affine([1.0, 0.0, 0.0, 1.0, x, y])
    }

    pub fn scale(x: f32, y: f32) -> Affine {
        Affine([x, 0.0, 0.0, y, 0.0, 0.0])
    }

    /// Clockwise on screen (the y axis points down), as CSS `rotate()`.
    pub fn rotate_deg(deg: f32) -> Affine {
        let (s, c) = deg.to_radians().sin_cos();
        Affine([c, s, -s, c, 0.0, 0.0])
    }

    /// CSS `skew(ax, ay)`.
    pub fn skew_deg(x_deg: f32, y_deg: f32) -> Affine {
        Affine([1.0, y_deg.to_radians().tan(), x_deg.to_radians().tan(), 1.0, 0.0, 0.0])
    }

    /// `self ∘ other`: apply `other` first, then `self`.
    pub fn then_after(&self, other: &Affine) -> Affine {
        let [a1, b1, c1, d1, e1, f1] = self.0;
        let [a2, b2, c2, d2, e2, f2] = other.0;
        Affine([
            a1 * a2 + c1 * b2,
            b1 * a2 + d1 * b2,
            a1 * c2 + c1 * d2,
            b1 * c2 + d1 * d2,
            a1 * e2 + c1 * f2 + e1,
            b1 * e2 + d1 * f2 + f1,
        ])
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// The inverse, if the transform is not degenerate.
    pub fn inverse(&self) -> Option<Affine> {
        let [a, b, c, d, e, f] = self.0;
        let det = a * d - b * c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let (ia, ib, ic, id) = (d / det, -b / det, -c / det, a / det);
        Some(Affine([ia, ib, ic, id, -(ia * e + ic * f), -(ib * e + id * f)]))
    }

    /// Whether it keeps rectangles axis-aligned (translation and scale).
    pub fn is_axis_aligned(&self) -> bool {
        self.0[1].abs() < 1e-6 && self.0[2].abs() < 1e-6
    }

    pub fn is_identity(&self) -> bool {
        self.0.iter().zip(Affine::IDENTITY.0).all(|(v, i)| (v - i).abs() < 1e-6)
    }

    pub fn is_finite(&self) -> bool {
        self.0.iter().all(|v| v.is_finite())
    }

    /// The bounding box `(x, y, w, h)` of the rectangle `(x, y, w, h)` mapped
    /// through this transform.
    pub fn map_rect(&self, (x, y, w, h): (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        let corners = [self.apply(x, y), self.apply(x + w, y), self.apply(x, y + h), self.apply(x + w, y + h)];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
        for (cx, cy) in corners {
            x0 = x0.min(cx);
            y0 = y0.min(cy);
            x1 = x1.max(cx);
            y1 = y1.max(cy);
        }
        (x0, y0, x1 - x0, y1 - y0)
    }

    /// The same transform for content shifted by (`dx`, `dy`) — what a
    /// scroll does to a transformed subtree: `S ∘ M ∘ S⁻¹`.
    pub fn conjugate_by_shift(&self, dx: f32, dy: f32) -> Affine {
        Affine::translate(dx, dy).then_after(self).then_after(&Affine::translate(-dx, -dy))
    }
}

/// A CSS transform, as `style` resolves it, around the pivot (`cx`, `cy`)
/// (`transform-origin: center`). `style` keeps the functions' values, not
/// their written order; they apply in the order of the CSS individual
/// transform properties — translate, rotate, then scale — with skew between
/// rotate and scale.
pub fn css_transform(t: &style::Transform, cx: f32, cy: f32) -> Affine {
    Affine::translate(cx + t.translate_x, cy + t.translate_y)
        .then_after(&Affine::rotate_deg(t.rotate_deg))
        .then_after(&Affine::skew_deg(t.skew_x_deg, t.skew_y_deg))
        .then_after(&Affine::scale(t.scale_x, t.scale_y))
        .then_after(&Affine::translate(-cx, -cy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4
    }

    #[test]
    fn composition_inverse_and_rectangles() {
        let m = Affine::translate(10.0, 5.0).then_after(&Affine::rotate_deg(90.0));
        assert!(close(m.apply(1.0, 0.0), (10.0, 6.0)), "rotate first, then translate: {:?}", m.apply(1.0, 0.0));
        let inv = m.inverse().unwrap();
        assert!(close(inv.apply(10.0, 6.0), (1.0, 0.0)));
        assert!(!m.is_axis_aligned() && Affine::scale(2.0, 3.0).is_axis_aligned());
        let (x, y, w, h) = Affine::rotate_deg(45.0).map_rect((0.0, 0.0, 1.0, 1.0));
        assert!((w - 2f32.sqrt()).abs() < 1e-4 && (h - 2f32.sqrt()).abs() < 1e-4 && (x + 2f32.sqrt() / 2.0).abs() < 1e-4 && y.abs() < 1e-4);
        assert!(Affine::scale(0.0, 1.0).inverse().is_none(), "degenerate");
    }

    #[test]
    fn a_css_transform_pivots_on_the_box_centre() {
        let t = style::Transform { rotate_deg: 90.0, ..style::Transform::default() };
        let m = css_transform(&t, 50.0, 50.0);
        assert!(close(m.apply(50.0, 50.0), (50.0, 50.0)), "the centre stays put");
        assert!(close(m.apply(100.0, 50.0), (50.0, 100.0)), "the right edge swings down (clockwise)");
        let t = style::Transform { translate_x: 5.0, scale_x: 2.0, scale_y: 2.0, ..style::Transform::default() };
        assert!(close(css_transform(&t, 10.0, 10.0).apply(20.0, 10.0), (35.0, 10.0)), "scale about the centre, then translate");
        assert!(css_transform(&style::Transform::default(), 3.0, 4.0).is_identity());
    }

    #[test]
    fn shifting_a_transformed_subtree_moves_it_rigidly() {
        let m = css_transform(&style::Transform { rotate_deg: 30.0, ..style::Transform::default() }, 20.0, 20.0);
        let shifted = m.conjugate_by_shift(0.0, -7.0);
        let p = m.apply(25.0, 11.0);
        assert!(close(shifted.apply(25.0, 11.0 - 7.0), (p.0, p.1 - 7.0)));
    }
}
