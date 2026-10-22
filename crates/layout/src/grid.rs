//! Milestone 33: CSS Grid layout (CSS Grid 1), in its common form.
//!
//! 1. **Items**: the container's in-flow children (generated content
//!    included); out-of-flow children only record a static position.
//! 2. **Explicit grid**: `grid-template-columns` / `-rows` (with
//!    `repeat(auto-fill | auto-fit, …)` repeated as often as fits) and
//!    `grid-template-areas`.
//! 3. **Placement** (§8): line numbers (negative from the end), `span`,
//!    named areas; then auto-placement, row by row, "sparse" — items with
//!    both lines first, then the rest in document order. Implicit tracks
//!    take `grid-auto-rows` / `-columns`.
//! 4. **Track sizing** (a simplified §11): each track's base size from its
//!    minimum (a length, or its items' min-content), grown towards its
//!    maximum (a length, or its items' max-content) while there is room,
//!    then `fr` tracks share what is left. Spanning items' excess is shared
//!    by the tracks they span. Columns first; rows from the items' heights
//!    at their column widths (`fr` rows need a definite container height).
//! 5. **Items** are laid out at their area's width and stretched to its
//!    height (or placed by `align-items`).
//!
//! Scope cuts: named lines, `grid-auto-flow: column`/`dense`, `justify-*`/
//! `align-content` on tracks, subgrid, baseline alignment.

use crate::box_model::{display_of, effective_border, is_out_of_flow, layout_element_sized, style_of, with_generated};
use crate::intrinsic::{IntrinsicCache, IntrinsicSizes};
use crate::reuse::translate;
use crate::LayoutBox;
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use style::{AlignItems, Display, GridLine, GridPlacement, Style, StyleCache, TrackBreadth, TrackList, TrackSize, MAX_GRID_TRACKS};
use text::Font;

/// Most rows auto-placement creates.
const MAX_GRID_ROWS: usize = 10_000;

struct Item<'a> {
    element: ElementRef<'a>,
    row: usize,
    col: usize,
    rows: usize,
    cols: usize,
}

/// The explicit tracks of a track list, `auto-fill`/`auto-fit` repeated as
/// often as they fit in `available` (once when it is unknown).
fn explicit_tracks(list: &TrackList, available: Option<f32>, gap: f32) -> Vec<TrackSize> {
    let mut tracks = list.tracks.clone();
    if let Some((at, repeated)) = &list.auto_repeat {
        let fixed = |t: &TrackSize| match (t.max, t.min) {
            (TrackBreadth::Fixed(l), _) | (_, TrackBreadth::Fixed(l)) => Some(l.resolve(available.unwrap_or(0.0))),
            _ => None,
        };
        let count = match (available, repeated.iter().map(fixed).collect::<Option<Vec<f32>>>()) {
            (Some(avail), Some(sizes)) => {
                let others: f32 = tracks.iter().filter_map(fixed).sum::<f32>() + gap * tracks.len() as f32;
                let one: f32 = sizes.iter().sum::<f32>() + gap * sizes.len() as f32;
                if one > 0.0 {
                    (((avail - others + gap) / one).floor().max(1.0) as usize).min(MAX_GRID_TRACKS / repeated.len().max(1))
                } else {
                    1
                }
            }
            _ => 1,
        };
        let insert: Vec<TrackSize> = (0..count).flat_map(|_| repeated.iter().copied()).collect();
        let at = (*at).min(tracks.len());
        tracks.splice(at..at, insert);
    }
    tracks.truncate(MAX_GRID_TRACKS);
    tracks
}

/// Where a named area is, as (row start, row end, column start, column
/// end), exclusive ends.
fn area(areas: &[Vec<String>], name: &str) -> Option<(usize, usize, usize, usize)> {
    let mut found: Option<(usize, usize, usize, usize)> = None;
    for (r, row) in areas.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell == name {
                found = Some(match found {
                    None => (r, r + 1, c, c + 1),
                    Some((r0, r1, c0, c1)) => (r0.min(r), r1.max(r + 1), c0.min(c), c1.max(c + 1)),
                });
            }
        }
    }
    found
}

/// One axis of an item's placement: `(start, span)`, `start` `None` when
/// auto-placed. `explicit` is the number of explicit tracks on the axis;
/// `named` resolves an area's range on it.
fn resolve_axis(p: &GridPlacement, explicit: usize, named: &dyn Fn(&str) -> Option<(usize, usize)>) -> (Option<usize>, usize) {
    let line = |l: &GridLine, is_end: bool| -> Option<usize> {
        match l {
            GridLine::Line(n) if *n > 0 => Some((*n - 1) as usize),
            GridLine::Line(n) => Some((explicit as i32 + 1 + *n).max(0) as usize),
            GridLine::Area(name) => named(name).map(|(s, e)| if is_end { e } else { s }),
            _ => None,
        }
    };
    let span_of = |l: &GridLine| match l {
        GridLine::Span(n) => *n as usize,
        _ => 1,
    };
    match (line(&p.start, false), line(&p.end, true)) {
        (Some(a), Some(b)) if a != b => (Some(a.min(b)), a.max(b) - a.min(b)),
        (Some(a), Some(_)) => (Some(a), 1),
        (Some(a), None) => (Some(a), span_of(&p.end)),
        (None, Some(b)) => {
            let span = span_of(&p.start);
            (Some(b.saturating_sub(span)), span)
        }
        (None, None) => (None, span_of(&p.start).max(span_of(&p.end))),
    }
}

/// Places every item on the grid (explicit lines first, then auto-placed
/// row by row). Returns the items and the grid's column and row counts.
fn place<'a>(elements: Vec<ElementRef<'a>>, style_cache: &StyleCache, container: &Style, explicit_cols: usize, explicit_rows: usize) -> (Vec<Item<'a>>, usize, usize) {
    let areas = &container.grid().template_areas;
    let named_rows = |n: &str| area(areas, n).map(|(r0, r1, _, _)| (r0, r1));
    let named_cols = |n: &str| area(areas, n).map(|(_, _, c0, c1)| (c0, c1));
    let mut cols = explicit_cols.max(1);
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut items = Vec::new();
    // (element, row start, row span, column start, column span)
    type Pending<'e> = (ElementRef<'e>, Option<usize>, usize, Option<usize>, usize);
    let mut pending: Vec<Pending<'a>> = Vec::new();
    for element in elements {
        let st = style_of(style_cache, element);
        let (row, rows) = resolve_axis(&st.grid().row, explicit_rows, &named_rows);
        let (col, colspan) = resolve_axis(&st.grid().column, explicit_cols, &named_cols);
        let rows = rows.clamp(1, MAX_GRID_ROWS);
        let colspan = colspan.clamp(1, MAX_GRID_TRACKS);
        if let Some(c) = col {
            cols = cols.max((c + colspan).min(MAX_GRID_TRACKS));
        }
        cols = cols.max(colspan.min(MAX_GRID_TRACKS));
        pending.push((element, row, rows, col, colspan));
    }
    let mark = |occupied: &mut Vec<Vec<bool>>, row: usize, rows: usize, col: usize, colspan: usize| {
        let end = (row + rows).min(MAX_GRID_ROWS);
        if occupied.len() < end {
            occupied.resize(end, vec![false; cols]);
        }
        for r in occupied.iter_mut().take(end).skip(row) {
            r.iter_mut().take((col + colspan).min(cols)).skip(col).for_each(|cell| *cell = true);
        }
    };
    let free = |occupied: &Vec<Vec<bool>>, row: usize, rows: usize, col: usize, colspan: usize| {
        col + colspan <= cols && (row..row + rows).all(|r| occupied.get(r).is_none_or(|cells| (col..col + colspan).all(|c| !cells[c])))
    };
    // Items with both lines, then the rest in order (sparse).
    let (definite, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|p| p.1.is_some() && p.3.is_some());
    for (element, row, rows, col, colspan) in definite {
        let (row, col) = (row.unwrap_or(0).min(MAX_GRID_ROWS - 1), col.unwrap_or(0));
        mark(&mut occupied, row, rows, col, colspan);
        items.push(Item { element, row, col, rows, cols: colspan });
    }
    let (mut cursor_row, mut cursor_col) = (0usize, 0usize);
    for (element, row, rows, col, colspan) in rest {
        let (r, c) = match (row, col) {
            (Some(r), None) => {
                let r = r.min(MAX_GRID_ROWS - 1);
                ((0..=cols.saturating_sub(colspan)).find(|c| free(&occupied, r, rows, *c, colspan)).map(|c| (r, c))).unwrap_or((r, 0))
            }
            (None, Some(c)) => ((0..MAX_GRID_ROWS).find(|r| free(&occupied, *r, rows, c, colspan)).unwrap_or(0), c),
            _ => {
                let mut found = (cursor_row, 0);
                'search: for r in cursor_row..MAX_GRID_ROWS {
                    let start = if r == cursor_row { cursor_col } else { 0 };
                    for c in start..=cols.saturating_sub(colspan) {
                        if free(&occupied, r, rows, c, colspan) {
                            found = (r, c);
                            break 'search;
                        }
                    }
                }
                (cursor_row, cursor_col) = (found.0, found.1 + colspan);
                found
            }
        };
        mark(&mut occupied, r, rows, c, colspan);
        items.push(Item { element, row: r, col: c, rows, cols: colspan });
    }
    let row_count = items.iter().map(|i| i.row + i.rows).max().unwrap_or(0).max(explicit_rows).min(MAX_GRID_ROWS);
    (items, cols, row_count)
}

/// An item's outer (margin-box) min- and max-content widths.
fn item_widths(item: ElementRef<'_>, intrinsic: &dyn crate::intrinsic::SizeLookup, style_cache: &StyleCache) -> (f32, f32) {
    let st = style_of(style_cache, item);
    let margins = st.margin.left.and_then(style::Length::absolute).unwrap_or(0.0) + st.margin.right.and_then(style::Length::absolute).unwrap_or(0.0);
    let extra = st.padding.left.px + st.padding.right.px + effective_border(st).horizontal() + margins;
    let sizes = intrinsic.sizes_of(item.node_id()).unwrap_or(IntrinsicSizes::ZERO);
    (sizes.min_content + extra, sizes.max_content + extra)
}

/// Sizes tracks (§11, simplified). `contributions[i]` is each item's
/// `(first track, span, min, max)` on this axis. `available` is `None`
/// when indefinite (then `fr` tracks behave as `auto`).
fn size_tracks(tracks: &[TrackSize], contributions: &[(usize, usize, f32, f32)], available: Option<f32>, gap: f32) -> Vec<f32> {
    let n = tracks.len();
    let basis = available.unwrap_or(0.0);
    let mut base = vec![0.0f32; n];
    let mut limit = vec![0.0f32; n];
    let mut fr = vec![0.0f32; n];
    let (mut item_min, mut item_max) = (vec![0.0f32; n], vec![0.0f32; n]);
    for &(first, span, min, max) in contributions {
        if span == 1 && first < n {
            item_min[first] = item_min[first].max(min);
            item_max[first] = item_max[first].max(max);
        }
    }
    for (i, t) in tracks.iter().enumerate() {
        base[i] = match t.min {
            TrackBreadth::Fixed(l) => l.resolve(basis).max(0.0),
            TrackBreadth::MaxContent => item_max[i],
            _ => item_min[i],
        };
        limit[i] = match t.max {
            TrackBreadth::Fixed(l) => l.resolve(basis).max(base[i]),
            TrackBreadth::MinContent => item_min[i].max(base[i]),
            TrackBreadth::Fr(f) => {
                fr[i] = f;
                if available.is_some() {
                    base[i]
                } else {
                    item_max[i].max(base[i])
                }
            }
            _ => item_max[i].max(base[i]),
        };
    }
    // A spanning item's excess minimum goes to the flexible-size tracks it
    // spans, evenly.
    for &(first, span, min, _) in contributions.iter().filter(|c| c.1 > 1) {
        let range = first.min(n)..(first + span).min(n);
        let have: f32 = base[range.clone()].iter().sum::<f32>() + gap * range.len().saturating_sub(1) as f32;
        let growable: Vec<usize> = range.clone().filter(|i| !matches!(tracks[*i].min, TrackBreadth::Fixed(_))).collect();
        if min > have && !growable.is_empty() {
            let each = (min - have) / growable.len() as f32;
            for i in growable {
                base[i] += each;
                limit[i] = limit[i].max(base[i]);
            }
        }
    }
    let gaps = gap * n.saturating_sub(1) as f32;
    let Some(available) = available else {
        return limit;
    };
    // Grow non-flexible tracks towards their limits, evenly.
    let mut free = available - gaps - base.iter().sum::<f32>();
    for _ in 0..n {
        let growing: Vec<usize> = (0..n).filter(|i| fr[*i] == 0.0 && base[*i] < limit[*i]).collect();
        if free <= 0.001 || growing.is_empty() {
            break;
        }
        let share = free / growing.len() as f32;
        for i in growing {
            let add = share.min(limit[i] - base[i]);
            base[i] += add;
            free -= add;
        }
    }
    // Flexible tracks share what is left (none of them below its base).
    let total_fr: f32 = fr.iter().sum();
    if total_fr > 0.0 && free > 0.0 {
        let flexible_base: f32 = (0..n).filter(|i| fr[*i] > 0.0).map(|i| base[i]).sum();
        let unit = (free + flexible_base) / total_fr.max(1.0);
        for i in (0..n).filter(|i| fr[*i] > 0.0) {
            base[i] = base[i].max(unit * fr[i]);
        }
    }
    base
}

/// The grid items among `children` (the container's children with its
/// generated content): in-flow, box-generating elements. Out-of-flow ones
/// record their static position when `reuse` is given.
fn items_of<'a>(
    element: ElementRef<'_>,
    children: &'a crate::box_model::WithGenerated<'a>,
    style_cache: &StyleCache,
    reuse: Option<(&mut crate::LayoutReuse, f32, f32)>,
) -> Vec<ElementRef<'a>> {
    let mut out = Vec::new();
    let mut reuse = reuse;
    for child in children.iter() {
        if let ChildRef::Element(e) = child {
            if matches!(display_of(style_cache, e), Display::None | Display::TableColumn) {
                continue;
            }
            if is_out_of_flow(style_cache, e) {
                if let Some((r, dx, dy)) = reuse.as_mut() {
                    r.record_static_position(e.node_id(), element.node_id(), *dx, *dy);
                }
                continue;
            }
            out.push(e);
        }
    }
    out
}

/// A grid container's content-box min- and max-content widths.
pub(crate) fn intrinsic_sizes(element: ElementRef<'_>, style: &Style, style_cache: &StyleCache, intrinsic: &dyn crate::intrinsic::SizeLookup) -> IntrinsicSizes {
    let children = with_generated(element, style_cache);
    let items = items_of(element, &children, style_cache, None);
    let columns = explicit_tracks(&style.grid().template_columns, None, style.gap);
    let explicit_cols = columns.len().max(style.grid().template_areas.first().map_or(0, Vec::len));
    let explicit_rows = explicit_tracks(&style.grid().template_rows, None, style.row_gap).len().max(style.grid().template_areas.len());
    let (placed, cols, _) = place(items, style_cache, style, explicit_cols, explicit_rows);
    let tracks: Vec<TrackSize> = (0..cols).map(|i| columns.get(i).copied().unwrap_or(style.grid().auto_columns)).collect();
    let contributions: Vec<(usize, usize, f32, f32)> =
        placed.iter().map(|it| {
            let (min, max) = item_widths(it.element, intrinsic, style_cache);
            (it.col, it.cols, min, max)
        }).collect();
    let gaps = style.gap * cols.saturating_sub(1) as f32;
    let max = size_tracks(&tracks, &contributions, None, style.gap).iter().sum::<f32>() + gaps;
    let min = size_tracks(&tracks, &contributions, Some(0.0), style.gap).iter().sum::<f32>() + gaps;
    IntrinsicSizes::new(min.min(max), max)
}

/// Lays out a grid container's items in its content box. Returns the boxes
/// and the height used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_grid(
    style: &Style,
    element: ElementRef<'_>,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    content_height: Option<f32>,
    border_box_origin: (f32, f32),
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let children = with_generated(element, style_cache);
    let items = items_of(element, &children, style_cache, Some((reuse, content_x - border_box_origin.0, content_y - border_box_origin.1)));
    let column_list = explicit_tracks(&style.grid().template_columns, Some(content_width), style.gap);
    let row_list = explicit_tracks(&style.grid().template_rows, content_height, style.row_gap);
    let explicit_cols = column_list.len().max(style.grid().template_areas.first().map_or(0, Vec::len));
    let explicit_rows = row_list.len().max(style.grid().template_areas.len());
    let (placed, cols, rows) = place(items, style_cache, style, explicit_cols, explicit_rows);
    if placed.is_empty() && rows == 0 {
        return (Vec::new(), 0.0);
    }

    // Columns.
    let column_tracks: Vec<TrackSize> = (0..cols).map(|i| column_list.get(i).copied().unwrap_or(style.grid().auto_columns)).collect();
    let contributions: Vec<(usize, usize, f32, f32)> = placed
        .iter()
        .map(|it| {
            let (min, max) = item_widths(it.element, intrinsic, style_cache);
            (it.col, it.cols, min, max)
        })
        .collect();
    let widths = size_tracks(&column_tracks, &contributions, Some(content_width), style.gap);
    let offsets = |sizes: &[f32], gap: f32| -> Vec<f32> {
        let mut out = Vec::with_capacity(sizes.len() + 1);
        let mut at = 0.0;
        for s in sizes {
            out.push(at);
            at += s + gap;
        }
        out.push((at - gap).max(0.0));
        out
    };
    let col_at = offsets(&widths, style.gap);
    let span_width = |col: usize, span: usize| {
        let end = (col + span).min(cols);
        (col_at[end] - col_at[col] - if end < cols { style.gap } else { 0.0 }).max(0.0)
    };

    // Items at their areas' widths; rows from their heights.
    let mut laid: Vec<LayoutBox> = Vec::with_capacity(placed.len());
    for it in &placed {
        let st = style_of(style_cache, it.element);
        let area_width = span_width(it.col, it.cols);
        let m = st.margin.resolve(area_width);
        // `justify-self: stretch` for an auto width.
        let forced = st.width.is_none().then_some((area_width - m.left - m.right).max(0.0));
        laid.push(layout_element_sized(it.element, 0.0, 0.0, area_width, forced, None, font, style_cache, intrinsic, image_cache, reuse, depth));
    }
    let row_tracks: Vec<TrackSize> = (0..rows).map(|i| row_list.get(i).copied().unwrap_or(style.grid().auto_rows)).collect();
    let row_contributions: Vec<(usize, usize, f32, f32)> = placed
        .iter()
        .zip(&laid)
        .map(|(it, b)| {
            let m = style_of(style_cache, it.element).margin.resolve(span_width(it.col, it.cols));
            let h = b.height + m.top + m.bottom;
            (it.row, it.rows, h, h)
        })
        .collect();
    let heights = size_tracks(&row_tracks, &row_contributions, content_height, style.row_gap);
    let row_at = offsets(&heights, style.row_gap);
    let span_height = |row: usize, span: usize| {
        let end = (row + span).min(rows);
        (row_at[end] - row_at[row] - if end < rows { style.row_gap } else { 0.0 }).max(0.0)
    };

    // Place each item in its area.
    let mut boxes = Vec::with_capacity(laid.len());
    for (it, mut b) in placed.iter().zip(laid) {
        let st = style_of(style_cache, it.element);
        let m = st.margin.resolve(span_width(it.col, it.cols));
        let area_height = span_height(it.row, it.rows);
        let free = (area_height - m.top - m.bottom - b.height).max(0.0);
        let dy = match style.align_items {
            AlignItems::Stretch => {
                if st.height.is_none() {
                    b.height += free;
                    b.touch();
                }
                0.0
            }
            AlignItems::FlexStart => 0.0,
            AlignItems::Center => free / 2.0,
            AlignItems::FlexEnd => free,
        };
        let x = content_x + col_at[it.col] + m.left;
        let y = content_y + row_at[it.row] + m.top + dy;
        let (rel_x, rel_y) = crate::box_model::relative_offset(st, span_width(it.col, it.cols));
        let (tx, ty) = (x + rel_x - b.x, y + rel_y - b.y);
        translate(&mut b, tx, ty);
        boxes.push(b);
    }
    (boxes, row_at[rows])
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::{Element, Node};
    use crate::layout_document;

    fn grid(html: &str, css: &str) -> (Element, LayoutBox) {
        let root = html_parser::parse_document(&format!("<html><body>{html}</body></html>")).unwrap();
        let body = root.children.iter().find_map(|n| match n { Node::Element(e) if e.tag == "body" => Some(e.clone()), _ => None }).unwrap();
        let laid = layout_document(&body, 800.0, &Font::dejavu_sans(), &css_parser::parse_stylesheet(css), &mut ImageCache::new());
        (body, laid)
    }

    fn find(b: &LayoutBox, key: dom::NodeId) -> Option<&LayoutBox> {
        if b.element_key == Some(key) && b.text.is_none() {
            return Some(b);
        }
        b.children.iter().find_map(|c| find(c, key))
    }

    fn at(doc: &(Element, LayoutBox), id: &str) -> (f32, f32, f32, f32) {
        let el = doc.0.find_by_id(id).unwrap();
        let b = find(&doc.1, el.node_id).unwrap();
        (b.x, b.y, b.width, b.height)
    }

    #[test]
    fn fixed_fr_and_gaps() {
        let doc = grid("<div id=g><div id=a>a</div><div id=b>b</div><div id=c>c</div><div id=d>d</div></div>",
            "#g { display: grid; grid-template-columns: 100px 1fr 2fr; gap: 10px 20px; width: 520px } div div { height: 30px }");
        assert_eq!(at(&doc, "a"), (0.0, 0.0, 100.0, 30.0));
        let unit = (520.0 - 100.0 - 2.0 * 20.0) / 3.0;
        assert!((at(&doc, "b").2 - unit).abs() < 0.01, "one fr of the 380px left");
        let c = at(&doc, "c");
        assert!((c.0 - (140.0 + unit)).abs() < 0.01 && (c.2 - 2.0 * unit).abs() < 0.01, "{c:?}");
        assert_eq!(at(&doc, "d"), (0.0, 40.0, 100.0, 30.0), "auto-placed on the next row, row gap 10");
        assert_eq!(at(&doc, "g").3, 70.0);
    }

    #[test]
    fn explicit_lines_spans_and_named_areas() {
        let doc = grid(
            "<div id=g><div id=head>h</div><div id=side>s</div><div id=main>m</div><div id=wide>w</div></div>",
            "#g { display: grid; width: 300px; grid-template-columns: 100px 200px; grid-template-areas: 'head head' 'side main'; grid-auto-rows: 20px } \
             #head { grid-area: head } #side { grid-area: side } #main { grid-area: main } #wide { grid-column: 1 / -1 }",
        );
        assert_eq!(at(&doc, "head"), (0.0, 0.0, 300.0, 20.0));
        assert_eq!(at(&doc, "side"), (0.0, 20.0, 100.0, 20.0));
        assert_eq!(at(&doc, "main"), (100.0, 20.0, 200.0, 20.0));
        assert_eq!(at(&doc, "wide"), (0.0, 40.0, 300.0, 20.0), "1 / -1 spans the explicit columns");
    }

    #[test]
    fn auto_fill_repeats_as_often_as_fits_and_rows_fit_their_content() {
        let doc = grid("<div id=g><p id=a>a</p><p id=b>b</p><p id=c>c</p><p id=d>d</p></div>",
            "#g { display: grid; width: 450px; grid-template-columns: repeat(auto-fill, minmax(100px, 1fr)) } p { margin: 0 }");
        assert_eq!(at(&doc, "a").2, 112.5, "four 100px columns fit, sharing 450px");
        assert_eq!(at(&doc, "d").0, 337.5);
        assert!(at(&doc, "a").3 > 10.0, "an auto row is as tall as its text");
    }

    #[test]
    fn hostile_placements_are_bounded() {
        let doc = grid("<div id=g><i id=a>a</i></div>", "#g { display: grid } #a { grid-column: 999999 / span 999999; grid-row: -999999 }");
        assert!(at(&doc, "a").0.is_finite());
    }
}
