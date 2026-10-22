//! Milestone 33: CSS Grid values — track lists, lines, template areas
//! (CSS Grid 1 §7–8).
//!
//! Bounded: at most [`MAX_GRID_TRACKS`] explicit tracks after `repeat()`
//! expansion, line numbers clamped to ±[`MAX_GRID_LINE`], `span` to
//! [`MAX_GRID_LINE`], template areas to [`MAX_GRID_TRACKS`] rows/columns.

use crate::length::{parse_length, LengthContext};
use crate::values::Length;

pub const MAX_GRID_TRACKS: usize = 1000;
pub const MAX_GRID_LINE: i32 = 1000;

/// Every CSS Grid property of one element (container and item ones).
#[derive(Debug, Clone, PartialEq)]
pub struct GridStyle {
    pub template_columns: TrackList,
    pub template_rows: TrackList,
    pub template_areas: Vec<Vec<String>>,
    pub auto_columns: TrackSize,
    pub auto_rows: TrackSize,
    pub column: GridPlacement,
    pub row: GridPlacement,
}

impl Default for GridStyle {
    fn default() -> Self {
        GridStyle {
            template_columns: TrackList::default(),
            template_rows: TrackList::default(),
            template_areas: Vec::new(),
            auto_columns: TrackSize::AUTO,
            auto_rows: TrackSize::AUTO,
            column: GridPlacement::default(),
            row: GridPlacement::default(),
        }
    }
}

/// One end of a track's sizing function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackBreadth {
    Fixed(Length),
    /// `<n>fr` (only valid as a maximum).
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// A track's `minmax(min, max)`; every other form maps onto one
/// (`100px` = `minmax(100px, 100px)`, `1fr` = `minmax(auto, 1fr)`,
/// `auto` = `minmax(auto, auto)`, `fit-content(x)` ≈ `minmax(auto, x)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackSize {
    pub min: TrackBreadth,
    pub max: TrackBreadth,
}

impl TrackSize {
    pub const AUTO: TrackSize = TrackSize { min: TrackBreadth::Auto, max: TrackBreadth::Auto };
}

/// `grid-template-columns` / `-rows`: explicit tracks, with at most one
/// `repeat(auto-fill | auto-fit, …)` whose count depends on the container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackList {
    pub tracks: Vec<TrackSize>,
    /// The auto-repeated tracks and where they go in `tracks`.
    pub auto_repeat: Option<(usize, Vec<TrackSize>)>,
}

/// One end of `grid-column` / `grid-row`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GridLine {
    #[default]
    Auto,
    /// A line number (1-based; negative counts from the end).
    Line(i32),
    Span(u32),
    /// A named area's edge (from `grid-area: name`).
    Area(String),
}

/// `grid-column` or `grid-row`: start / end.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GridPlacement {
    pub start: GridLine,
    pub end: GridLine,
}

fn breadth(token: &str, ctx: &LengthContext) -> Option<TrackBreadth> {
    let t = token.trim().to_ascii_lowercase();
    match t.as_str() {
        "auto" => return Some(TrackBreadth::Auto),
        "min-content" => return Some(TrackBreadth::MinContent),
        "max-content" => return Some(TrackBreadth::MaxContent),
        _ => {}
    }
    if let Some(fr) = t.strip_suffix("fr") {
        return fr.trim().parse::<f32>().ok().filter(|f| f.is_finite() && *f >= 0.0).map(|f| TrackBreadth::Fr(f.min(1e6)));
    }
    parse_length(&t, ctx).filter(|l| l.px >= 0.0 && l.percent >= 0.0).map(TrackBreadth::Fixed)
}

/// One track size (`100px`, `1fr`, `minmax(…)`, `fit-content(…)`).
fn track_size(token: &str, ctx: &LengthContext) -> Option<TrackSize> {
    let t = token.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(args) = lower.strip_prefix("minmax(").and_then(|a| a.strip_suffix(')')) {
        let (min, max) = args.split_once(',')?;
        let (min, max) = (breadth(min, ctx)?, breadth(max, ctx)?);
        if matches!(min, TrackBreadth::Fr(_)) {
            return None; // a flexible minimum is invalid
        }
        return Some(TrackSize { min, max });
    }
    if let Some(arg) = lower.strip_prefix("fit-content(").and_then(|a| a.strip_suffix(')')) {
        let limit = parse_length(arg, ctx)?;
        return Some(TrackSize { min: TrackBreadth::Auto, max: TrackBreadth::Fixed(limit) });
    }
    match breadth(t, ctx)? {
        TrackBreadth::Fr(f) => Some(TrackSize { min: TrackBreadth::Auto, max: TrackBreadth::Fr(f) }),
        b => Some(TrackSize { min: b, max: b }),
    }
}

/// Splits a track list into its top-level components (keeping `(…)`
/// together; `[line-names]` are dropped — named lines are not supported).
fn components(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, None);
    let mut in_names = false;
    for (i, c) in value.char_indices() {
        match c {
            '[' if depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(&value[s..i]);
                }
                in_names = true;
            }
            ']' if depth == 0 => in_names = false,
            _ if in_names => {}
            '(' => {
                depth += 1;
                start.get_or_insert(i);
            }
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(&value[s..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push(&value[s..]);
    }
    out
}

/// `grid-template-columns` / `-rows`. `none` is an empty list.
pub fn parse_track_list(value: &str, ctx: &LengthContext) -> Option<TrackList> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Some(TrackList::default());
    }
    let mut list = TrackList::default();
    for component in components(value) {
        let lower = component.to_ascii_lowercase();
        if let Some(args) = lower.strip_prefix("repeat(").and_then(|a| a.strip_suffix(')')) {
            let (count, tracks) = args.split_once(',')?;
            let tracks: Vec<TrackSize> = components(tracks).into_iter().map(|t| track_size(t, ctx)).collect::<Option<_>>()?;
            if tracks.is_empty() {
                return None;
            }
            match count.trim() {
                "auto-fill" | "auto-fit" => {
                    if list.auto_repeat.is_some() {
                        return None;
                    }
                    list.auto_repeat = Some((list.tracks.len(), tracks));
                }
                n => {
                    let n: usize = n.parse().ok().filter(|n| *n >= 1)?;
                    if n.saturating_mul(tracks.len()).saturating_add(list.tracks.len()) > MAX_GRID_TRACKS {
                        return None;
                    }
                    for _ in 0..n {
                        list.tracks.extend_from_slice(&tracks);
                        if list.tracks.len() > MAX_GRID_TRACKS {
                            return None;
                        }
                    }
                }
            }
        } else {
            list.tracks.push(track_size(component, ctx)?);
        }
        if list.tracks.len() > MAX_GRID_TRACKS {
            return None;
        }
    }
    Some(list)
}

/// One implicit track size (`grid-auto-rows` / `-columns`; the first of a
/// list).
pub fn parse_auto_track(value: &str, ctx: &LengthContext) -> Option<TrackSize> {
    components(value).first().and_then(|t| track_size(t, ctx))
}

fn grid_line(token: &str) -> Option<GridLine> {
    let t = token.trim();
    let lower = t.to_ascii_lowercase();
    if lower == "auto" {
        return Some(GridLine::Auto);
    }
    if let Some(n) = lower.strip_prefix("span") {
        let n = n.trim();
        let n: u32 = if n.is_empty() { 1 } else { n.parse().ok()? };
        return (n >= 1).then_some(GridLine::Span(n.min(MAX_GRID_LINE as u32)));
    }
    if let Ok(n) = lower.parse::<i32>() {
        return (n != 0).then_some(GridLine::Line(n.clamp(-MAX_GRID_LINE, MAX_GRID_LINE)));
    }
    let ident = t.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') && !t.is_empty();
    ident.then(|| GridLine::Area(t.to_string()))
}

/// `grid-column` / `grid-row`: `<start> [/ <end>]`.
pub fn parse_placement(value: &str) -> Option<GridPlacement> {
    let mut parts = value.split('/');
    let start = grid_line(parts.next()?)?;
    let end = match parts.next() {
        Some(e) => grid_line(e)?,
        None => match &start {
            GridLine::Area(name) => GridLine::Area(name.clone()),
            _ => GridLine::Auto,
        },
    };
    if parts.next().is_some() {
        return None;
    }
    Some(GridPlacement { start, end })
}

/// `grid-area`: a name, or `row-start / column-start / row-end /
/// column-end`. Returns (row, column).
pub fn parse_grid_area(value: &str) -> Option<(GridPlacement, GridPlacement)> {
    let parts: Vec<GridLine> = value.split('/').map(grid_line).collect::<Option<_>>()?;
    let area = |i: usize, fallback: &GridLine| match parts.get(i) {
        Some(l) => l.clone(),
        None => match fallback {
            GridLine::Area(n) => GridLine::Area(n.clone()),
            _ => GridLine::Auto,
        },
    };
    match parts.len() {
        1..=4 => {
            let row_start = parts[0].clone();
            let col_start = area(1, &row_start);
            let row_end = area(2, &row_start);
            let col_end = area(3, &col_start);
            Some((GridPlacement { start: row_start, end: row_end }, GridPlacement { start: col_start, end: col_end }))
        }
        _ => None,
    }
}

/// `grid-template-areas`: one string per row, names separated by spaces
/// (`.` for an empty cell). `None` if the rows differ in length.
pub fn parse_template_areas(value: &str) -> Option<Vec<Vec<String>>> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let mut rows = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest[open..].chars().next()?;
        let body = &rest[open + 1..];
        let close = body.find(quote)?;
        let row: Vec<String> = body[..close].split_whitespace().map(str::to_string).collect();
        if row.is_empty() || row.len() > MAX_GRID_TRACKS || rows.len() >= MAX_GRID_TRACKS {
            return None;
        }
        rows.push(row);
        rest = &body[close + 1..];
    }
    let width = rows.first()?.len();
    rows.iter().all(|r| r.len() == width).then_some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: LengthContext = LengthContext::DEFAULT;

    #[test]
    fn track_lists() {
        let l = parse_track_list("100px 1fr minmax(50px, 2fr) auto repeat(2, 10% 1em)", &CTX).unwrap();
        assert_eq!(l.tracks.len(), 8);
        assert_eq!(l.tracks[1], TrackSize { min: TrackBreadth::Auto, max: TrackBreadth::Fr(1.0) });
        assert_eq!(l.tracks[2].min, TrackBreadth::Fixed(Length::px(50.0)));
        assert_eq!(l.tracks[7].max, TrackBreadth::Fixed(Length::px(16.0)));
        let a = parse_track_list("[full-start] repeat(auto-fill, minmax(200px, 1fr)) [full-end]", &CTX).unwrap();
        assert_eq!(a.auto_repeat.as_ref().map(|(at, t)| (*at, t.len())), Some((0, 1)));
        for bad in ["1fr minmax(1fr, 2fr)", "repeat(0, 1px)", "repeat(2)", "-5px", "wobble", "repeat(auto-fill, 1px) repeat(auto-fit, 1px)"] {
            assert_eq!(parse_track_list(bad, &CTX), None, "{bad}");
        }
        assert_eq!(parse_track_list("repeat(100000, 1px)", &CTX), None, "bounded");
    }

    #[test]
    fn placements_and_areas() {
        assert_eq!(parse_placement("1 / 3"), Some(GridPlacement { start: GridLine::Line(1), end: GridLine::Line(3) }));
        assert_eq!(parse_placement("span 2"), Some(GridPlacement { start: GridLine::Span(2), end: GridLine::Auto }));
        assert_eq!(parse_placement("1 / -1").map(|p| p.end), Some(GridLine::Line(-1)));
        assert_eq!(parse_placement("0"), None);
        let (row, col) = parse_grid_area("main").unwrap();
        assert_eq!(row.start, GridLine::Area("main".into()));
        assert_eq!(col.end, GridLine::Area("main".into()));
        let (row, col) = parse_grid_area("1 / 2 / 3 / 4").unwrap();
        assert_eq!((row.end, col.start), (GridLine::Line(3), GridLine::Line(2)));
        let areas = parse_template_areas(r#""head head" "side main""#).unwrap();
        assert_eq!(areas, vec![vec!["head", "head"], vec!["side", "main"]]);
        assert_eq!(parse_template_areas(r#""a b" "c""#), None, "ragged");
    }
}
