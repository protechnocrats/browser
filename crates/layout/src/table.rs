//! Milestone 33: tables — CSS 2 §17, the automatic table layout algorithm
//! (§17.5.2.2) in its common form.
//!
//! The table's children are sorted into captions, row groups, rows and
//! cells (loose cells get an implicit row; anything else in a row is
//! treated as a cell). Cells are placed on a grid honouring `colspan` and
//! `rowspan`. Each column's minimum and maximum widths come from its cells'
//! min-/max-content widths (a spanning cell's excess is shared among the
//! columns it spans); the table's width is distributed over them. Cells are
//! laid out at their columns' width, a row is as tall as its tallest cell
//! (a spanning cell's excess goes to its last row), and a cell's content is
//! placed by its `vertical-align`. Rows and row groups get boxes of their
//! own, so their backgrounds paint behind the cells.
//!
//! Scope cuts: `table-layout: fixed` is laid out automatically; collapsing
//! borders are treated as zero spacing (both cells' borders are drawn);
//! `<col>` widths are ignored; footers stay in document order.

use crate::box_model::{display_of, effective_border, layout_element, layout_element_sized, style_of};
use crate::intrinsic::{IntrinsicCache, IntrinsicSizes};
use crate::reuse::translate;
use crate::LayoutBox;
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use style::{Display, Style, StyleCache};
use text::Font;

/// Most columns and rows one table lays out (the rest are dropped).
pub(crate) const MAX_TABLE_COLUMNS: usize = 1000;
pub(crate) const MAX_TABLE_ROWS: usize = 65_536;
/// Largest `colspan` / `rowspan` honoured (HTML §4.9.11 caps them at
/// 1000 and 65534).
const MAX_SPAN: usize = 1000;

pub(crate) struct Cell<'a> {
    pub element: ElementRef<'a>,
    pub row: usize,
    pub col: usize,
    pub rowspan: usize,
    pub colspan: usize,
}

pub(crate) struct Row<'a> {
    /// `None` for an implicit row around loose cells.
    pub element: Option<ElementRef<'a>>,
    /// The row group (index into `TableGrid::groups`) it belongs to.
    pub group: Option<usize>,
}

pub(crate) struct TableGrid<'a> {
    pub captions: Vec<ElementRef<'a>>,
    pub groups: Vec<ElementRef<'a>>,
    pub rows: Vec<Row<'a>>,
    pub cells: Vec<Cell<'a>>,
    pub columns: usize,
    /// Absolutely positioned / fixed children of the table's parts: not
    /// part of the grid.
    pub out_of_flow: Vec<ElementRef<'a>>,
}

fn span(element: ElementRef<'_>, name: &str) -> usize {
    element.attr(name).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, MAX_SPAN)
}

impl<'a> TableGrid<'a> {
    pub(crate) fn build(table: ElementRef<'a>, style_cache: &StyleCache) -> TableGrid<'a> {
        let mut grid = TableGrid { captions: Vec::new(), groups: Vec::new(), rows: Vec::new(), cells: Vec::new(), columns: 0, out_of_flow: Vec::new() };
        // Which grid slots are taken by cells spanning down from above.
        let mut taken: Vec<Vec<bool>> = Vec::new();
        // In-flow children only; out-of-flow ones are set aside.
        let mut out_of_flow = Vec::new();
        let mut elements = |parent: ElementRef<'a>| -> Vec<ElementRef<'a>> {
            let mut kept = Vec::new();
            for e in parent.children().filter_map(|c| match c {
                ChildRef::Element(e) => Some(e),
                ChildRef::Text(_) => None,
            }) {
                if crate::box_model::is_out_of_flow(style_cache, e) {
                    out_of_flow.push(e);
                } else {
                    kept.push(e);
                }
            }
            kept
        };
        let mut loose: Vec<ElementRef<'a>> = Vec::new();
        let flush_loose = |grid: &mut TableGrid<'a>, taken: &mut Vec<Vec<bool>>, loose: &mut Vec<ElementRef<'a>>| {
            if !loose.is_empty() {
                let cells = std::mem::take(loose);
                grid.add_row(None, None, cells, taken, style_cache);
            }
        };
        for child in elements(table) {
            match display_of(style_cache, child) {
                Display::None | Display::TableColumn => {}
                Display::TableCaption => grid.captions.push(child),
                Display::TableRowGroup => {
                    flush_loose(&mut grid, &mut taken, &mut loose);
                    let group = grid.groups.len();
                    grid.groups.push(child);
                    for row in elements(child) {
                        match display_of(style_cache, row) {
                            Display::None | Display::TableColumn => {}
                            Display::TableRow => {
                                let cells: Vec<ElementRef<'a>> = elements(row).into_iter().filter(|c| !matches!(display_of(style_cache, *c), Display::None | Display::TableColumn)).collect();
                                grid.add_row(Some(row), Some(group), cells, &mut taken, style_cache);
                            }
                            // A cell directly in a group: its own row.
                            _ => grid.add_row(None, Some(group), vec![row], &mut taken, style_cache),
                        }
                    }
                }
                Display::TableRow => {
                    flush_loose(&mut grid, &mut taken, &mut loose);
                    let cells: Vec<ElementRef<'a>> = elements(child).into_iter().filter(|c| !matches!(display_of(style_cache, *c), Display::None | Display::TableColumn)).collect();
                    grid.add_row(Some(child), None, cells, &mut taken, style_cache);
                }
                _ => loose.push(child),
            }
        }
        flush_loose(&mut grid, &mut taken, &mut loose);
        grid.out_of_flow = out_of_flow;
        grid
    }

    fn add_row(&mut self, element: Option<ElementRef<'a>>, group: Option<usize>, cells: Vec<ElementRef<'a>>, taken: &mut Vec<Vec<bool>>, _style_cache: &StyleCache) {
        if self.rows.len() >= MAX_TABLE_ROWS {
            diagnostics::guard(diagnostics::Guard::LayoutDepth, "table has more rows than laid out; the rest dropped");
            return;
        }
        let row = self.rows.len();
        self.rows.push(Row { element, group });
        if taken.len() <= row {
            taken.resize(row + 1, Vec::new());
        }
        let mut col = 0;
        for cell in cells {
            while taken[row].get(col).copied().unwrap_or(false) {
                col += 1;
            }
            if col >= MAX_TABLE_COLUMNS {
                break;
            }
            let colspan = span(cell, "colspan").min(MAX_TABLE_COLUMNS - col);
            let rowspan = span(cell, "rowspan").min(MAX_TABLE_ROWS - row);
            for r in row..row + rowspan.min(64) {
                if taken.len() <= r {
                    taken.resize(r + 1, Vec::new());
                }
                if taken[r].len() < col + colspan {
                    taken[r].resize(col + colspan, false);
                }
                taken[r][col..col + colspan].iter_mut().for_each(|t| *t = true);
            }
            self.cells.push(Cell { element: cell, row, col, rowspan, colspan });
            col += colspan;
            self.columns = self.columns.max(col);
        }
    }
}

/// A cell's outer (border-box) min- and max-content widths, from the
/// intrinsic pass.
fn cell_widths(cell: ElementRef<'_>, style: &Style, intrinsic: &dyn crate::intrinsic::SizeLookup) -> (f32, f32) {
    let extra = style.padding.left.px + style.padding.right.px + effective_border(style).horizontal();
    let sizes = intrinsic.sizes_of(cell.node_id()).unwrap_or(IntrinsicSizes::ZERO);
    (sizes.min_content + extra, sizes.max_content + extra)
}

/// Each column's minimum and maximum width (§17.5.2.2).
fn column_ranges(grid: &TableGrid, style_cache: &StyleCache, intrinsic: &dyn crate::intrinsic::SizeLookup) -> (Vec<f32>, Vec<f32>) {
    let n = grid.columns;
    let (mut mins, mut maxs) = (vec![0.0f32; n], vec![0.0f32; n]);
    // Single-column cells first, then spanning ones share their excess.
    let mut cells: Vec<&Cell> = grid.cells.iter().collect();
    cells.sort_by_key(|c| c.colspan);
    for cell in cells {
        let style = style_of(style_cache, cell.element);
        let (min, max) = cell_widths(cell.element, style, intrinsic);
        let cols = cell.col..(cell.col + cell.colspan).min(n);
        if cols.is_empty() {
            continue;
        }
        let count = cols.len() as f32;
        let (have_min, have_max): (f32, f32) = (mins[cols.clone()].iter().sum(), maxs[cols.clone()].iter().sum());
        for c in cols {
            if min > have_min {
                mins[c] += (min - have_min) / count;
            }
            if max > have_max {
                maxs[c] += (max - have_max) / count;
            }
            maxs[c] = maxs[c].max(mins[c]);
        }
    }
    (mins, maxs)
}

/// The table's content-box min- and max-content widths — its intrinsic
/// sizes, from its cells' (already in `intrinsic`).
pub(crate) fn intrinsic_sizes(table: ElementRef<'_>, style: &Style, style_cache: &StyleCache, intrinsic: &dyn crate::intrinsic::SizeLookup) -> IntrinsicSizes {
    let grid = TableGrid::build(table, style_cache);
    let (mins, maxs) = column_ranges(&grid, style_cache, intrinsic);
    let spacing = spacing(style).0 * (grid.columns + 1) as f32;
    IntrinsicSizes::new(mins.iter().sum::<f32>() + spacing, maxs.iter().sum::<f32>() + spacing)
}

fn spacing(style: &Style) -> (f32, f32) {
    if style.table_borders.collapse {
        (0.0, 0.0)
    } else {
        style.table_borders.spacing
    }
}

/// Distributes `available` px over columns with these ranges: everything
/// beyond the maxima in proportion to them, between minima and maxima
/// evenly by fraction, and the minima when there is less room than that.
fn distribute(mins: &[f32], maxs: &[f32], available: f32) -> Vec<f32> {
    let (sum_min, sum_max): (f32, f32) = (mins.iter().sum(), maxs.iter().sum());
    if available >= sum_max {
        let extra = available - sum_max;
        if sum_max > 0.0 {
            maxs.iter().map(|m| m + extra * m / sum_max).collect()
        } else {
            vec![available / mins.len().max(1) as f32; mins.len()]
        }
    } else if available > sum_min && sum_max > sum_min {
        let t = (available - sum_min) / (sum_max - sum_min);
        mins.iter().zip(maxs).map(|(lo, hi)| lo + (hi - lo) * t).collect()
    } else {
        mins.to_vec()
    }
}

/// A box for a row or row group: its own background and border, no content
/// of its own.
fn part_box(element: ElementRef<'_>, style: &Style, x: f32, y: f32, width: f32, height: f32, children: Vec<LayoutBox>) -> LayoutBox {
    LayoutBox {
        x,
        y,
        width,
        height,
        background_color: style.background_color,
        border: style.has_border().then(|| Box::new(style.border().used())),
        element_key: Some(element.node_id()),
        opacity: style.opacity,
        visible: style.visibility == style::Visibility::Visible,
        children,
        ..LayoutBox::empty()
    }
}

/// Lays out a table's content (captions, then the grid) in its content box
/// at (`content_x`, `content_y`), `content_width` wide. Returns the boxes
/// and the height used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_table(
    table_style: &Style,
    table: ElementRef<'_>,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    border_box_origin: (f32, f32),
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let grid = TableGrid::build(table, style_cache);
    // Out-of-flow children would have been at the start of the table.
    for el in &grid.out_of_flow {
        reuse.record_static_position(el.node_id(), table.node_id(), content_x - border_box_origin.0, content_y - border_box_origin.1);
    }
    let (space_x, space_y) = spacing(table_style);
    let mut boxes = Vec::new();
    let mut cursor_y = content_y;
    for caption in &grid.captions {
        let b = layout_element(*caption, content_x, cursor_y, content_width, font, style_cache, intrinsic, image_cache, reuse, depth);
        cursor_y = b.y + b.height;
        boxes.push(b);
    }
    if grid.columns == 0 || grid.rows.is_empty() {
        return (boxes, cursor_y - content_y);
    }

    let (mins, maxs) = column_ranges(&grid, style_cache, intrinsic);
    let widths = distribute(&mins, &maxs, (content_width - space_x * (grid.columns + 1) as f32).max(0.0));
    let mut col_x = Vec::with_capacity(grid.columns + 1);
    let mut x = content_x + space_x;
    for w in &widths {
        col_x.push(x);
        x += w + space_x;
    }
    col_x.push(x);

    // Cells at their widths (placed at the origin for now), and each row's
    // height from its single-row cells.
    let mut laid: Vec<LayoutBox> = Vec::with_capacity(grid.cells.len());
    let mut row_heights = vec![0.0f32; grid.rows.len()];
    for cell in &grid.cells {
        let end = (cell.col + cell.colspan).min(grid.columns);
        let width = (col_x[end] - space_x - col_x[cell.col]).max(0.0);
        let b = layout_element_sized(cell.element, 0.0, 0.0, width, Some(width), None, font, style_cache, intrinsic, image_cache, reuse, depth + 2);
        if cell.rowspan == 1 {
            row_heights[cell.row] = row_heights[cell.row].max(b.height);
        }
        laid.push(b);
    }
    // A spanning cell taller than its rows grows its last row.
    for (cell, b) in grid.cells.iter().zip(&laid) {
        if cell.rowspan > 1 {
            let last = (cell.row + cell.rowspan).min(grid.rows.len()) - 1;
            let spanned: f32 = row_heights[cell.row..=last].iter().sum::<f32>() + space_y * (last - cell.row) as f32;
            if b.height > spanned {
                row_heights[last] += b.height - spanned;
            }
        }
    }
    let mut row_y = Vec::with_capacity(grid.rows.len() + 1);
    let mut y = cursor_y + space_y;
    for h in &row_heights {
        row_y.push(y);
        y += h + space_y;
    }
    row_y.push(y);

    // Place each cell: stretched to its rows' height, content by
    // `vertical-align`.
    let mut row_cells: Vec<Vec<LayoutBox>> = (0..grid.rows.len()).map(|_| Vec::new()).collect();
    for (cell, mut b) in grid.cells.iter().zip(laid) {
        let last = (cell.row + cell.rowspan).min(grid.rows.len()) - 1;
        let height = row_y[last] + row_heights[last] - row_y[cell.row];
        let (dx, dy) = (col_x[cell.col] - b.x, row_y[cell.row] - b.y);
        translate(&mut b, dx, dy);
        let free = (height - b.height).max(0.0);
        let offset = match style_of(style_cache, cell.element).vertical_align {
            style::VerticalAlign::Middle => free / 2.0,
            style::VerticalAlign::Bottom | style::VerticalAlign::TextBottom => free,
            _ => 0.0,
        };
        for child in b.children.iter_mut() {
            translate(child, 0.0, offset);
        }
        b.height = height;
        b.touch(); // possibly moved over from the previous frame (ADR 022)
        row_cells[cell.row].push(b);
    }

    // Rows (and row groups) as boxes holding their cells.
    let row_x = content_x + space_x;
    let row_width = (col_x[grid.columns] - space_x - row_x).max(0.0);
    let mut group_rows: Vec<Vec<LayoutBox>> = (0..grid.groups.len()).map(|_| Vec::new()).collect();
    let mut direct: Vec<LayoutBox> = Vec::new();
    for (i, (row, cells)) in grid.rows.iter().zip(row_cells).enumerate() {
        let row_box = match row.element {
            Some(el) => part_box(el, style_of(style_cache, el), row_x, row_y[i], row_width, row_heights[i], cells),
            None => LayoutBox { x: row_x, y: row_y[i], width: row_width, height: row_heights[i], children: cells, ..LayoutBox::empty() },
        };
        match row.group {
            Some(g) => group_rows[g].push(row_box),
            None => direct.push(row_box),
        }
    }
    for (group, rows) in grid.groups.iter().zip(group_rows) {
        if rows.is_empty() {
            continue;
        }
        let top = rows.iter().map(|r| r.y).fold(f32::INFINITY, f32::min);
        let bottom = rows.iter().map(|r| r.y + r.height).fold(f32::NEG_INFINITY, f32::max);
        boxes.push(part_box(*group, style_of(style_cache, *group), row_x, top, row_width, bottom - top, rows));
    }
    boxes.extend(direct);
    let used = row_y[grid.rows.len()] - content_y;
    (boxes, used.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::Node;

    #[test]
    fn widths_fill_by_max_share_interpolate_or_fall_back_to_minimums() {
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 160.0), vec![40.0, 120.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 50.0), vec![15.0, 35.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 5.0), vec![10.0, 10.0]);
        assert_eq!(distribute(&[0.0, 0.0], &[0.0, 0.0], 100.0), vec![50.0, 50.0]);
    }

    use crate::layout_document;

    fn table(html: &str, css: &str) -> (dom::Element, LayoutBox) {
        let root = html_parser::parse_document(&format!("<html><body>{html}</body></html>")).unwrap();
        let body = root.children.iter().find_map(|n| match n { Node::Element(e) if e.tag == "body" => Some(e.clone()), _ => None }).unwrap();
        let sheet = css_parser::parse_stylesheet(css).with_user_agent(style::user_agent_stylesheet());
        let laid = layout_document(&body, 800.0, &Font::dejavu_sans(), &sheet, &mut ImageCache::new());
        (body, laid)
    }

    fn find(b: &LayoutBox, key: dom::NodeId) -> Option<&LayoutBox> {
        if b.element_key == Some(key) && b.text.is_none() {
            return Some(b);
        }
        b.children.iter().find_map(|c| find(c, key))
    }

    /// A copy of the box of the element with `id`.
    fn cell(doc: &(dom::Element, LayoutBox), id: &str) -> LayoutBox {
        let el = doc.0.find_by_id(id).unwrap();
        find(&doc.1, el.node_id).cloned().unwrap_or_else(|| panic!("no box for #{id}"))
    }

    #[test]
    fn cells_share_rows_and_columns() {
        let doc = table(
            "<table><tr><td id=a>short</td><td id=b>a much longer cell of text</td></tr><tr><td id=c>x</td><td id=d>y</td></tr></table>",
            "table { border-spacing: 0 } td { padding: 0 }",
        );
        let (a, b, c, d) = (cell(&doc, "a"), cell(&doc, "b"), cell(&doc, "c"), cell(&doc, "d"));
        assert_eq!(a.y, b.y, "same row");
        assert_eq!(a.x, c.x, "same column");
        assert_eq!((a.width, b.width), (c.width, d.width), "columns are shared");
        assert!(b.width > a.width, "the longer content gets the wider column");
        assert_eq!(c.y, a.y + a.height, "the next row starts below");
        let t = cell(&doc, "a");
        assert!(t.width < 400.0, "an auto-width table shrinks to its content");
    }

    #[test]
    fn colspan_rowspan_spacing_and_vertical_align() {
        let doc = table(
            "<table><tr><td id=wide colspan=2>wide</td><td id=tall rowspan=2>tall</td></tr><tr><td id=l>l</td><td id=r>r</td></tr></table>",
            "table { border-spacing: 4px; width: 400px } td { padding: 0; vertical-align: top } #tall { vertical-align: middle }",
        );
        let (wide, l, r, tall) = (cell(&doc, "wide"), cell(&doc, "l"), cell(&doc, "r"), cell(&doc, "tall"));
        assert_eq!(wide.x, l.x);
        assert!((wide.x + wide.width - (r.x + r.width)).abs() < 0.01, "colspan covers both columns");
        assert_eq!(r.x - (l.x + l.width), 4.0, "border-spacing between columns");
        assert_eq!(tall.y, wide.y);
        assert!((tall.y + tall.height - (l.y + l.height)).abs() < 0.01, "rowspan covers both rows");
        let text = &tall.children[0];
        assert!(text.y > tall.y + 1.0, "middle-aligned content is pushed down");
        let spanned: f32 = tall.x + tall.width - wide.x;
        assert!((spanned - (400.0 - 8.0)).abs() < 0.5, "a width fills the table minus its edge spacing: {spanned}");
    }

    #[test]
    fn rows_paint_their_backgrounds_and_legacy_attributes_apply() {
        let doc = table(
            "<table id=t border=1 cellspacing=0 cellpadding=5><tr id=row bgcolor=#ff0000><td id=a>x</td></tr></table>",
            "",
        );
        let row = cell(&doc, "row");
        assert_eq!(row.background_color, style::Color { r: 255, g: 0, b: 0, a: 255 });
        let a = cell(&doc, "a");
        assert_eq!(a.border.as_ref().map(|b| b.top.width), Some(1.0), "border=1 gives cells a border");
        assert!((a.children[0].x - a.x - 6.0).abs() < 0.01, "cellpadding 5 + border 1");
        assert_eq!(cell(&doc, "t").border.as_ref().map(|b| b.top.width), Some(1.0));
    }

    #[test]
    fn display_table_on_divs_and_a_table_with_no_rows() {
        let doc = table("<div class=t><div class=r><div class=c id=a>a</div><div class=c id=b>b</div></div></div><table id=empty></table>",
            ".t { display: table } .r { display: table-row } .c { display: table-cell }");
        assert_eq!(cell(&doc, "a").y, cell(&doc, "b").y);
        assert!(cell(&doc, "b").x > cell(&doc, "a").x);
        assert_eq!(cell(&doc, "empty").height, 0.0);
    }

    #[test]
    fn hostile_spans_are_bounded() {
        let doc = table("<table><tr><td id=a colspan=999999999 rowspan=999999999>x</td></tr></table>", "");
        // colspan and rowspan are capped (1000 columns here: the table
        // overflows its container, as in CSS, but stays bounded).
        let w = cell(&doc, "a").width;
        assert!(w.is_finite() && w < 1000.0 * 3.0 + 800.0, "{w}");
    }
}
