//! Turning a frame's pass plan into render passes (Milestone 34) — pure, so
//! it is tested without a GPU.
//!
//! A frame draws into the **back buffer** (kept across frames, so a frame
//! with a damage rectangle redraws only that), each group into a **layer**
//! (a target the size of the back buffer, one per nesting depth, reused), and
//! finally copies the back buffer to the window. [`segment`] splits
//! `rendering::PassOp`s into [`Segment`]s — one render pass each: a target,
//! whether to clear it, a scissor, and the commands drawn in it.

use rendering::{DrawBatch, PassOp};

/// Groups nested deeper than this draw straight into their parent (a
/// hostile scene cannot make the renderer hold a layer per item).
pub const MAX_LAYER_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Back,
    Layer(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Fill the scissor with the canvas colour (a damage redraw).
    Background,
    Draw(DrawBatch),
    /// Draw layer `layer` onto this target: composite instance `index`.
    Composite { index: usize, layer: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub target: Target,
    pub clear: bool,
    /// `[x0, y0, x1, y1]` in world (CSS) px; `None`: the whole target.
    pub scissor: Option<[f32; 4]>,
    pub commands: Vec<Command>,
}

/// A group's layer drawn onto its parent: where, and at what opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Composite {
    pub bounds: [f32; 4],
    pub opacity: f32,
}

fn intersect(a: Option<[f32; 4]>, b: [f32; 4]) -> [f32; 4] {
    match a {
        None => b,
        Some(a) => [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]).max(a[0].max(b[0])), a[3].min(b[3]).max(a[1].max(b[1]))],
    }
}

/// Splits `ops` into render passes. `damage`: redraw only this rectangle of
/// the back buffer (which holds the previous frame); `None`: redraw all.
pub fn segment(ops: &[PassOp], damage: Option<[f32; 4]>) -> (Vec<Segment>, Vec<Composite>) {
    let mut segments = vec![Segment {
        target: Target::Back,
        clear: damage.is_none(),
        scissor: damage,
        commands: if damage.is_some() { vec![Command::Background] } else { Vec::new() },
    }];
    let mut composites = Vec::new();
    // Open groups: (layer, opacity, scissor of that layer).
    let mut open: Vec<(usize, f32, [f32; 4])> = Vec::new();
    let mut flattened = 0usize;
    for op in ops {
        match *op {
            PassOp::Draw(batch) => segments.last_mut().expect("never empty").commands.push(Command::Draw(batch)),
            PassOp::BeginGroup { opacity, bounds } => {
                if open.len() >= MAX_LAYER_DEPTH {
                    flattened += 1;
                    continue;
                }
                let parent_scissor = open.last().map(|g| g.2).or(damage);
                let scissor = intersect(parent_scissor, bounds);
                let layer = open.len();
                open.push((layer, opacity, scissor));
                segments.push(Segment { target: Target::Layer(layer), clear: true, scissor: Some(scissor), commands: Vec::new() });
            }
            PassOp::EndGroup => {
                if flattened > 0 {
                    flattened -= 1;
                    continue;
                }
                let Some((layer, opacity, scissor)) = open.pop() else { continue };
                let index = composites.len();
                composites.push(Composite { bounds: scissor, opacity });
                let (target, parent_scissor) = match open.last() {
                    Some(g) => (Target::Layer(g.0), Some(g.2)),
                    None => (Target::Back, damage),
                };
                segments.push(Segment { target, clear: false, scissor: parent_scissor, commands: vec![Command::Composite { index, layer }] });
            }
        }
    }
    // Groups left open (a malformed plan) are dropped, not drawn half-way.
    (segments, composites)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rendering::BatchKind;

    fn draw(n: u32) -> PassOp {
        PassOp::Draw(DrawBatch { kind: BatchKind::Rects, first: n, count: 1 })
    }

    #[test]
    fn groups_draw_into_layers_and_composite_back() {
        let ops = [
            draw(0),
            PassOp::BeginGroup { opacity: 0.5, bounds: [0.0, 0.0, 10.0, 10.0] },
            draw(1),
            PassOp::BeginGroup { opacity: 0.25, bounds: [2.0, 2.0, 20.0, 20.0] },
            draw(2),
            PassOp::EndGroup,
            draw(3),
            PassOp::EndGroup,
            draw(4),
        ];
        let (segments, composites) = segment(&ops, None);
        let targets: Vec<Target> = segments.iter().map(|s| s.target).collect();
        assert_eq!(targets, vec![Target::Back, Target::Layer(0), Target::Layer(1), Target::Layer(0), Target::Back]);
        assert!(segments[1].clear && segments[2].clear && !segments[3].clear && !segments[4].clear);
        assert_eq!(segments[2].scissor, Some([2.0, 2.0, 10.0, 10.0]), "a nested layer is clipped to its parent's");
        assert_eq!(segments[3].commands[0], Command::Composite { index: 0, layer: 1 });
        assert_eq!(segments[3].commands[1], Command::Draw(DrawBatch { kind: BatchKind::Rects, first: 3, count: 1 }), "drawing resumes in the parent");
        assert_eq!(composites, vec![Composite { bounds: [2.0, 2.0, 10.0, 10.0], opacity: 0.25 }, Composite { bounds: [0.0, 0.0, 10.0, 10.0], opacity: 0.5 }]);
    }

    #[test]
    fn a_damage_redraw_is_scissored_and_starts_with_the_background() {
        let ops = [draw(0), PassOp::BeginGroup { opacity: 0.5, bounds: [0.0, 0.0, 100.0, 100.0] }, draw(1), PassOp::EndGroup];
        let (segments, _) = segment(&ops, Some([10.0, 10.0, 20.0, 20.0]));
        assert!(!segments[0].clear);
        assert_eq!(segments[0].commands[0], Command::Background);
        assert_eq!(segments[0].scissor, Some([10.0, 10.0, 20.0, 20.0]));
        assert_eq!(segments[1].scissor, Some([10.0, 10.0, 20.0, 20.0]), "layers only redraw the damage too");
    }

    #[test]
    fn nesting_past_the_limit_draws_into_the_parent() {
        let mut ops = Vec::new();
        for _ in 0..MAX_LAYER_DEPTH + 5 {
            ops.push(PassOp::BeginGroup { opacity: 0.9, bounds: [0.0, 0.0, 1.0, 1.0] });
        }
        ops.push(draw(0));
        for _ in 0..MAX_LAYER_DEPTH + 5 {
            ops.push(PassOp::EndGroup);
        }
        let (segments, composites) = segment(&ops, None);
        assert!(segments.iter().all(|s| !matches!(s.target, Target::Layer(l) if l >= MAX_LAYER_DEPTH)));
        assert_eq!(composites.len(), MAX_LAYER_DEPTH);
    }
}
