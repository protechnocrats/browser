//! Milestone 33: floats (CSS 2 §9.5).
//!
//! A placed float is an [`Exclusion`]: the part of its block formatting
//! context that line boxes (in its container and in the container's
//! descendant blocks) flow around, and that later floats stack against.
//! The active exclusions live on `LayoutReuse` (the per-frame layout
//! context): a container pushes its floats as it places them and drops
//! them when it is done; a box that establishes a new block formatting
//! context starts with none.
//!
//! Simplification: a block container always grows to contain the floats it
//! placed (what the ubiquitous "clearfix" does); in CSS a float can hang out
//! of a parent that is not a formatting-context root.

/// One float's margin box, and which side it floats to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Exclusion {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub left: bool,
}

/// Most floats one container places (a float past it is laid out in flow).
pub(crate) const MAX_FLOATS: usize = 1024;

fn overlaps(e: &Exclusion, y: f32, h: f32) -> bool {
    e.y0 < y + h.max(0.01) && e.y1 > y
}

/// The horizontal room between the floats beside the band `y .. y+h`,
/// within `left ..= right`.
pub(crate) fn line_interval(exclusions: &[Exclusion], y: f32, h: f32, left: f32, right: f32) -> (f32, f32) {
    let mut interval = (left, right);
    for e in exclusions.iter().filter(|e| overlaps(e, y, h)) {
        if e.left {
            interval.0 = interval.0.max(e.x1);
        } else {
            interval.1 = interval.1.min(e.x0);
        }
    }
    interval
}

/// The nearest float bottom below `y` among those beside `y .. y+h` — where
/// to try next when something does not fit beside them.
pub(crate) fn next_bottom(exclusions: &[Exclusion], y: f32, h: f32) -> Option<f32> {
    exclusions.iter().filter(|e| overlaps(e, y, h)).map(|e| e.y1).filter(|b| *b > y).reduce(f32::min)
}

/// Where `clear` puts a box: below every float on the cleared sides.
pub(crate) fn clearance(exclusions: &[Exclusion], clear: style::Clear) -> Option<f32> {
    exclusions.iter().filter(|e| (e.left && clear.left) || (!e.left && clear.right)).map(|e| e.y1).reduce(f32::max)
}

/// Where a float with a `w` x `h` margin box goes (its top-left corner): as
/// high as possible from `y_min` (never above an earlier float), then as
/// far to its side as the other floats allow, moving down while it does not
/// fit (CSS 2 §9.5.1). A float wider than the container is placed at the
/// first y with no floats beside it.
pub(crate) fn place(exclusions: &[Exclusion], left_edge: f32, right_edge: f32, y_min: f32, w: f32, h: f32, left: bool) -> (f32, f32) {
    let mut y = exclusions.iter().map(|e| e.y0).fold(y_min, f32::max);
    for _ in 0..=exclusions.len() {
        let (l, r) = line_interval(exclusions, y, h, left_edge, right_edge);
        if r - l >= w - 0.01 {
            return (if left { l } else { r - w }, y);
        }
        match next_bottom(exclusions, y, h) {
            Some(next) => y = next,
            None => return (if left { l } else { r - w }, y),
        }
    }
    (if left { left_edge } else { right_edge - w }, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(x0: f32, y0: f32, x1: f32, y1: f32, left: bool) -> Exclusion {
        Exclusion { x0, y0, x1, y1, left }
    }

    #[test]
    fn floats_stack_side_by_side_then_drop_below() {
        let mut floats = Vec::new();
        for _ in 0..3 {
            let (x, y) = place(&floats, 0.0, 100.0, 0.0, 40.0, 10.0, true);
            floats.push(ex(x, y, x + 40.0, y + 10.0, true));
        }
        assert_eq!(floats.iter().map(|f| (f.x0, f.y0)).collect::<Vec<_>>(), vec![(0.0, 0.0), (40.0, 0.0), (0.0, 10.0)]);
        let (x, y) = place(&floats, 0.0, 100.0, 0.0, 30.0, 5.0, false);
        assert_eq!((x, y), (70.0, 10.0), "a right float never above the last float's top");
    }

    #[test]
    fn lines_narrow_beside_floats_and_clearance_goes_below() {
        let floats = [ex(0.0, 0.0, 30.0, 50.0, true), ex(80.0, 20.0, 100.0, 40.0, false)];
        assert_eq!(line_interval(&floats, 0.0, 10.0, 0.0, 100.0), (30.0, 100.0));
        assert_eq!(line_interval(&floats, 25.0, 10.0, 0.0, 100.0), (30.0, 80.0));
        assert_eq!(line_interval(&floats, 60.0, 10.0, 0.0, 100.0), (0.0, 100.0));
        assert_eq!(next_bottom(&floats, 25.0, 10.0), Some(40.0));
        assert_eq!(clearance(&floats, style::Clear { left: true, right: false }), Some(50.0));
        assert_eq!(clearance(&floats, style::Clear { left: false, right: true }), Some(40.0));
    }

    #[test]
    fn a_float_wider_than_the_room_goes_below_the_floats() {
        let floats = [ex(0.0, 0.0, 60.0, 30.0, true)];
        assert_eq!(place(&floats, 0.0, 100.0, 0.0, 50.0, 10.0, true), (0.0, 30.0));
    }
}
