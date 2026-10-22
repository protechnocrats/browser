//! Milestone 10: flex container layout — a genuinely different
//! algorithm from block stacking (`crate::box_model::layout_element`'s
//! normal path), dispatched to when `display: flex` is set.
//!
//! **Row direction is fully implemented**: `flex-grow`/`flex-shrink`
//! actually resize items along the main axis, `justify-content` and
//! `align-items` both work, `gap` is respected.
//!
//! **Column direction stacks correctly with `justify-content`/
//! `align-items`/`gap`, but does NOT apply `flex-grow`/`flex-shrink`**
//! — items keep their natural content-driven height. A deliberate,
//! documented scope reduction given this milestone's effort budget,
//! not an oversight: row is the overwhelmingly more common direction
//! in real usage, and getting it fully correct mattered more than
//! symmetric-but-shallow support for both.
//!
//! Text nodes directly inside a flex container are skipped (real CSS
//! would wrap them in an anonymous flex item — implementing anonymous
//! box generation is real, separate work not undertaken here; direct
//! text children of a flex container are a genuinely rare pattern in
//! practice, almost everything is wrapped in an element).
//!
//! Flex items' own `margin` is not specially incorporated into the
//! main/cross-axis distribution math (each item's box model is still
//! computed correctly via the normal `layout_element` call — only the
//! *flex positioning* math ignores margin, relying on `gap` for
//! inter-item spacing instead, which is how most real flex layouts
//! actually space things anyway).

use crate::box_model::{effective_border, layout_element, layout_element_sized, style_of};
use crate::intrinsic::IntrinsicCache;
use crate::reuse::translate;
use crate::LayoutBox;
use dom::{ChildRef, ElementRef};
use images::ImageCache;
use style::{AlignContent, AlignItems, BoxSizing, FlexDirection, FlexWrap, JustifyContent, Style, StyleCache};
use text::Font;

#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_flex_children(
    container_style: &Style,
    element: ElementRef<'_>,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    explicit_content_height: Option<f32>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let generated = crate::box_model::with_generated(element, style_cache);
    let mut item_elements: Vec<ElementRef<'_>> = generated
        .iter()
        .filter_map(|c| match c {
            ChildRef::Element(el) if crate::box_model::display_of(style_cache, el) != style::Display::None => Some(el),
            _ => None,
        })
        // An out-of-flow child is not a flex item (CSS Flexbox §4.1).
        .filter(|el| !crate::box_model::is_out_of_flow(style_cache, *el))
        .collect();

    if item_elements.is_empty() {
        return (Vec::new(), 0.0);
    }

    // Compute each item's `Style` exactly ONCE (`style_of` clones a
    // whole `Style` — custom-properties map, transitions `Vec`, etc. —
    // not a cheap `Copy`); this used to be computed twice, once just to
    // read `.order` and again immediately after for `item_styles`.
    let mut item_styles: Vec<&Style> = item_elements.iter().map(|el| style_of(style_cache, *el)).collect();

    // Phase D: `order` — flex items are laid out (and painted) in
    // ascending `order`, ties broken by document order (a STABLE sort,
    // matching real CSS's tie-break rule). Reorders both
    // `item_elements` and the already-computed `item_styles` together
    // (via zip/unzip, no extra `style_of` calls) rather than
    // recomputing styles after reordering elements.
    if item_styles.iter().any(|s| s.order != 0) {
        let mut paired: Vec<(ElementRef<'_>, &Style)> = item_elements.into_iter().zip(item_styles).collect();
        paired.sort_by_key(|(_, s)| s.order);
        let (sorted_elements, sorted_styles): (Vec<ElementRef<'_>>, Vec<&Style>) = paired.into_iter().unzip();
        item_elements = sorted_elements;
        item_styles = sorted_styles;
    }

    if container_style.flex_direction == FlexDirection::Row {
        layout_flex_row(
            container_style,
            &item_elements,
            &item_styles,
            content_x,
            content_y,
            content_width,
            explicit_content_height,
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        )
    } else {
        layout_flex_column(
            container_style,
            &item_elements,
            &item_styles,
            content_x,
            content_y,
            content_width,
            explicit_content_height,
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        )
    }
}

/// One flex item's main-axis (horizontal) sizing inputs, border-box.
struct ItemSizing {
    /// The flex base size (CSS Flexbox §9.2 step 3).
    base: f32,
    /// Min/max main size: `min-width` (or the automatic minimum, the
    /// item's min-content size) and `max-width`.
    min: f32,
    max: f32,
    /// Horizontal margins, `auto` ones as zero.
    margin_left: f32,
    margin_right: f32,
    auto_margins: usize,
    auto_left: bool,
}

impl ItemSizing {
    fn new(style: &Style, element: ElementRef<'_>, content_width: f32, intrinsic: &IntrinsicCache) -> ItemSizing {
        let padding = style.padding.resolve(content_width);
        let extra = padding.left + padding.right + effective_border(style).horizontal();
        let to_border_box = |l: style::Length| {
            let v = l.resolve(content_width).max(0.0);
            match style.box_sizing {
                BoxSizing::BorderBox => v,
                BoxSizing::ContentBox => v + extra,
            }
        };
        let sizes = intrinsic.get(&element.node_id()).copied();
        let specified = style.flex_basis.or(style.width).map(to_border_box);
        let base = specified.unwrap_or_else(|| sizes.map_or(0.0, |z| z.max_content) + extra);
        // The automatic minimum size (§4.5): the min-content size, but no
        // larger than a specified width.
        let min = match style.limits().min_width {
            Some(l) => to_border_box(l),
            None => {
                let content_min = sizes.map_or(0.0, |z| z.content_min) + extra;
                style.width.map(to_border_box).map_or(content_min, |w| w.min(content_min))
            }
        };
        let max = style.limits().max_width.map(to_border_box).unwrap_or(f32::INFINITY).max(min);
        let margin = style.margin.resolve(content_width);
        let (auto_left, auto_right) = (style.margin.left.is_none(), style.margin.right.is_none());
        ItemSizing {
            base,
            min,
            max,
            margin_left: margin.left,
            margin_right: margin.right,
            auto_margins: usize::from(auto_left) + usize::from(auto_right),
            auto_left,
        }
    }

    fn clamp(&self, size: f32) -> f32 {
        size.min(self.max).max(self.min)
    }

    fn margins(&self) -> f32 {
        self.margin_left + self.margin_right
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_flex_row(
    container_style: &Style,
    items: &[ElementRef<'_>],
    item_styles: &[&Style],
    content_x: f32,
    content_y: f32,
    content_width: f32,
    explicit_content_height: Option<f32>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let gap = container_style.gap;
    let sizing: Vec<ItemSizing> = items.iter().zip(item_styles).map(|(el, st)| ItemSizing::new(st, *el, content_width, intrinsic)).collect();

    // Partition item indices into flex lines. `nowrap` => one line;
    // otherwise start a new line whenever the next item's outer
    // hypothetical size wouldn't fit (each line keeps at least one item).
    let lines: Vec<Vec<usize>> = if container_style.flex_wrap == FlexWrap::Nowrap {
        vec![(0..items.len()).collect()]
    } else {
        let mut lines: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut used = 0.0f32;
        for (i, item) in sizing.iter().enumerate() {
            let w = item.clamp(item.base) + item.margins();
            let with_gap = if current.is_empty() { w } else { used + gap + w };
            if !current.is_empty() && with_gap > content_width {
                lines.push(std::mem::take(&mut current));
                used = w;
                current.push(i);
            } else {
                used = with_gap;
                current.push(i);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    };

    // With a single flex line and an explicit container height, that
    // line's cross size IS the container's inner cross size (CSS flexbox
    // §9.4) — so align-items centers within the full height, not just
    // the tallest item.
    let single_line_cross = if lines.len() == 1 { explicit_content_height } else { None };

    // Lay out each line at a provisional y; we'll shift them for
    // align-content afterward.
    let mut line_results: Vec<(Vec<LayoutBox>, f32)> = Vec::with_capacity(lines.len());
    for line in &lines {
        let r = layout_flex_line(
            container_style,
            items,
            item_styles,
            &sizing,
            line,
            content_x,
            content_y,
            content_width,
            single_line_cross,
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        );
        line_results.push(r);
    }

    // Between wrapped lines: the row gap.
    let line_gap = container_style.row_gap;
    let natural_total: f32 = line_results.iter().map(|(_, h)| *h).sum::<f32>()
        + line_gap * line_results.len().saturating_sub(1) as f32;
    let container_cross = explicit_content_height.unwrap_or(natural_total);

    // Phase D: align-content distributes the lines in the cross axis.
    let leftover = (container_cross - natural_total).max(0.0);
    let line_count = line_results.len();
    let (mut cursor_y, extra_between, stretch_add) = match container_style.align_content {
        AlignContent::FlexStart => (content_y, 0.0, 0.0),
        AlignContent::Center => (content_y + leftover / 2.0, 0.0, 0.0),
        AlignContent::FlexEnd => (content_y + leftover, 0.0, 0.0),
        AlignContent::SpaceBetween if line_count > 1 => (content_y, leftover / (line_count - 1) as f32, 0.0),
        AlignContent::SpaceBetween => (content_y, 0.0, 0.0),
        AlignContent::SpaceAround if line_count > 0 => {
            let unit = leftover / line_count as f32;
            (content_y + unit / 2.0, unit, 0.0)
        }
        AlignContent::SpaceAround => (content_y, 0.0, 0.0),
        AlignContent::Stretch => (content_y, 0.0, if line_count > 0 { leftover / line_count as f32 } else { 0.0 }),
    };

    let mut order: Vec<usize> = (0..line_count).collect();
    if container_style.flex_wrap == FlexWrap::WrapReverse {
        order.reverse();
    }

    let mut boxes: Vec<LayoutBox> = Vec::new();
    for (pass_idx, &li) in order.iter().enumerate() {
        let (line_boxes, line_h) = &mut line_results[li];
        let line_h_final = *line_h + stretch_add;
        for b in line_boxes.iter_mut() {
            // The whole item moves, its contents with it.
            translate(b, 0.0, cursor_y - content_y);
            if container_style.align_items == AlignItems::Stretch && stretch_add > 0.0 {
                b.height += stretch_add;
                b.touch();
            }
        }
        boxes.extend(std::mem::take(line_boxes));
        cursor_y += line_h_final + line_gap;
        if pass_idx + 1 < line_count {
            cursor_y += extra_between;
        }
    }

    (boxes, container_cross)
}

/// CSS Flexbox §9.7 "Resolving Flexible Lengths": each item's main size
/// on one line — flex base sizes grown or shrunk into the free space, in
/// proportion to `flex-grow` / `flex-shrink × base`, with items that hit
/// their min or max frozen there and the rest redistributed.
fn resolve_flexible_lengths(sizing: &[&ItemSizing], grow: &[f32], shrink: &[f32], available: f32) -> Vec<f32> {
    let n = sizing.len();
    let hypothetical: Vec<f32> = sizing.iter().map(|s| s.clamp(s.base)).collect();
    let outer_hypothetical: f32 = hypothetical.iter().zip(sizing).map(|(h, s)| h + s.margins()).sum();
    let growing = outer_hypothetical < available;
    let mut target = hypothetical.clone();
    let mut frozen: Vec<bool> = (0..n)
        .map(|k| {
            let factor = if growing { grow[k] } else { shrink[k] };
            factor == 0.0 || (growing && sizing[k].base > hypothetical[k]) || (!growing && sizing[k].base < hypothetical[k])
        })
        .collect();
    // Each round freezes at least one item, so n + 1 rounds suffice.
    for _ in 0..=n {
        if frozen.iter().all(|f| *f) {
            break;
        }
        let used: f32 = (0..n).map(|k| sizing[k].margins() + if frozen[k] { target[k] } else { sizing[k].base }).sum();
        let free = available - used;
        let unfrozen = || (0..n).filter(|k| !frozen[*k]);
        let mut unclamped = target.clone();
        if growing {
            let total: f32 = unfrozen().map(|k| grow[k]).sum();
            // Factors summing below 1 take only that fraction of the space.
            let share = if total < 1.0 { free * total } else { free };
            for k in unfrozen() {
                unclamped[k] = sizing[k].base + if total > 0.0 { share * grow[k] / total } else { 0.0 };
            }
        } else {
            let total: f32 = unfrozen().map(|k| shrink[k] * sizing[k].base).sum();
            for k in unfrozen() {
                let scaled = shrink[k] * sizing[k].base;
                unclamped[k] = sizing[k].base + if total > 0.0 { free * scaled / total } else { 0.0 };
            }
        }
        let mut violation = 0.0;
        for k in unfrozen() {
            target[k] = sizing[k].clamp(unclamped[k]).max(0.0);
            violation += target[k] - unclamped[k];
        }
        let ks: Vec<usize> = unfrozen().collect();
        for k in ks {
            let freeze = if violation.abs() < 0.001 {
                true
            } else if violation > 0.0 {
                target[k] > unclamped[k]
            } else {
                target[k] < unclamped[k]
            };
            frozen[k] |= freeze;
        }
    }
    target
}

/// Lays out ONE flex line (row direction): resolves the items' main sizes,
/// lays each out at its size, then places them — `auto` margins first
/// absorb the free space, otherwise `justify-content` distributes it — and
/// aligns them in the cross axis. Positions boxes with `y` relative to
/// `line_y`; the caller shifts whole lines for `align-content`.
#[allow(clippy::too_many_arguments)]
fn layout_flex_line(
    container_style: &Style,
    all_items: &[ElementRef<'_>],
    all_styles: &[&Style],
    all_sizing: &[ItemSizing],
    line: &[usize],
    content_x: f32,
    line_y: f32,
    content_width: f32,
    cross_override: Option<f32>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let n = line.len();
    let gap = container_style.gap;
    let total_gap = if n > 1 { gap * (n - 1) as f32 } else { 0.0 };
    let sizing: Vec<&ItemSizing> = line.iter().map(|&i| &all_sizing[i]).collect();
    let grow: Vec<f32> = line.iter().map(|&i| all_styles[i].flex_grow.max(0.0)).collect();
    let shrink: Vec<f32> = line.iter().map(|&i| all_styles[i].flex_shrink.max(0.0)).collect();
    let widths = resolve_flexible_lengths(&sizing, &grow, &shrink, content_width - total_gap);

    // Each item laid out at its resolved width (its content reflows).
    let mut laid: Vec<LayoutBox> = Vec::with_capacity(n);
    for k in 0..n {
        let i = line[k];
        laid.push(layout_element_sized(
            all_items[i],
            content_x,
            line_y,
            content_width,
            Some(widths[k]),
            None,
            font,
            style_cache,
            intrinsic,
            image_cache,
            reuse,
            depth,
        ));
    }
    let vertical_margins = |i: usize| {
        let m = all_styles[i].margin.resolve(content_width);
        (m.top, m.bottom)
    };

    let line_cross = cross_override.unwrap_or_else(|| {
        (0..n).map(|k| {
            let (top, bottom) = vertical_margins(line[k]);
            laid[k].height + top + bottom
        })
        .fold(0.0, f32::max)
    });

    let used: f32 = (0..n).map(|k| widths[k] + sizing[k].margins()).sum::<f32>() + total_gap;
    let free = content_width - used;
    let auto_margin_count: usize = sizing.iter().map(|s| s.auto_margins).sum();
    // `auto` margins take the free space (§9.5 step 12), before
    // justify-content, which then has none left.
    let auto_share = if free > 0.0 && auto_margin_count > 0 { free / auto_margin_count as f32 } else { 0.0 };
    let extra_free_space = if auto_margin_count > 0 { 0.0 } else { free.max(0.0) };
    let (start_x, item_gap) = match container_style.justify_content {
        JustifyContent::FlexStart => (content_x, gap),
        JustifyContent::Center => (content_x + extra_free_space / 2.0, gap),
        JustifyContent::FlexEnd => (content_x + extra_free_space, gap),
        JustifyContent::SpaceBetween if n > 1 => (content_x, gap + extra_free_space / (n - 1) as f32),
        JustifyContent::SpaceBetween => (content_x, gap),
    };

    let mut cursor_x = start_x;
    let mut boxes = Vec::with_capacity(n);
    for (k, mut b) in laid.into_iter().enumerate() {
        let item = sizing[k];
        let (margin_top, margin_bottom) = vertical_margins(line[k]);
        let left = item.margin_left + if item.auto_left { auto_share } else { 0.0 };
        let right_auto = item.auto_margins - usize::from(item.auto_left);
        let right = item.margin_right + auto_share * right_auto as f32;
        let x = cursor_x + left;
        let y = match container_style.align_items {
            AlignItems::Stretch | AlignItems::FlexStart => line_y + margin_top,
            AlignItems::Center => line_y + margin_top + (line_cross - margin_top - margin_bottom - b.height) / 2.0,
            AlignItems::FlexEnd => line_y + line_cross - margin_bottom - b.height,
        };
        // Placed in flow, then any `position: relative` shift on top.
        let (rel_x, rel_y) = crate::box_model::relative_offset(all_styles[line[k]], content_width);
        let (dx, dy) = (x + rel_x - b.x, y + rel_y - b.y);
        translate(&mut b, dx, dy);
        // `stretch` fills the line — only an item whose height is auto.
        if container_style.align_items == AlignItems::Stretch && all_styles[line[k]].height.is_none() {
            b.height = (line_cross - margin_top - margin_bottom).max(b.height);
            b.touch();
        }
        cursor_x = x + widths[k] + right + item_gap;
        boxes.push(b);
    }

    (boxes, line_cross)
}

#[allow(clippy::too_many_arguments)]
fn layout_flex_column(
    container_style: &Style,
    items: &[ElementRef<'_>],
    item_styles: &[&Style],
    content_x: f32,
    content_y: f32,
    content_width: f32,
    explicit_content_height: Option<f32>,
    font: &Font,
    style_cache: &StyleCache,
    intrinsic: &IntrinsicCache,
    image_cache: &mut ImageCache,
    reuse: &mut crate::LayoutReuse,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let n = items.len();
    // A column's items are separated by the row gap.
    let gap = container_style.row_gap;

    // Natural stacking pass — cross axis (width) `stretch` (the
    // default) already falls out for free: an item with no explicit
    // width already fills `content_width` via the normal box-model
    // path, same as any block child.
    let mut boxes: Vec<LayoutBox> = Vec::with_capacity(n);
    let mut cursor_y = content_y;
    // (x offset of each item's `position: relative` shift, kept through
    // alignment below.)
    let mut relative_x: Vec<f32> = Vec::with_capacity(n);
    for (item_el, item_style) in items.iter().zip(item_styles) {
        let laid_out = layout_element(*item_el, content_x, cursor_y, content_width, font, style_cache, intrinsic, image_cache, reuse, depth);
        let (rel_x, rel_y) = crate::box_model::relative_offset(item_style, content_width);
        let margin_bottom = item_style.margin.resolve(content_width).bottom;
        cursor_y = laid_out.y - rel_y + laid_out.height + margin_bottom + gap;
        relative_x.push(rel_x);
        boxes.push(laid_out);
    }
    let mut natural_total_height = if n > 0 { (cursor_y - gap - content_y).max(0.0) } else { 0.0 };

    // Milestone 33: with a definite height the column's items grow and
    // shrink (CSS Flexbox §9.7, on the vertical axis): flex base size from
    // `flex-basis` or the natural height, the automatic minimum is the
    // content height, then each item is laid out again at its height.
    if let (Some(available), true) = (explicit_content_height, n > 0) {
        let sizing: Vec<ItemSizing> = boxes
            .iter()
            .zip(item_styles)
            .map(|(b, st)| {
                let m = st.margin.resolve(content_width);
                let natural = b.height;
                let base = st.flex_basis.map_or(natural, |l| l.resolve(available).max(0.0));
                let min = st.limits().min_height.and_then(style::Length::absolute).unwrap_or(if st.height.is_some() { 0.0 } else { natural.min(base.max(natural)) });
                let max = st.limits().max_height.and_then(style::Length::absolute).unwrap_or(f32::INFINITY).max(min);
                ItemSizing { base, min, max, margin_left: m.top, margin_right: m.bottom, auto_margins: 0, auto_left: false }
            })
            .collect();
        let grow: Vec<f32> = item_styles.iter().map(|st| st.flex_grow.max(0.0)).collect();
        let shrink: Vec<f32> = item_styles.iter().map(|st| st.flex_shrink.max(0.0)).collect();
        let refs: Vec<&ItemSizing> = sizing.iter().collect();
        let heights = resolve_flexible_lengths(&refs, &grow, &shrink, available - gap * (n - 1) as f32);
        if heights.iter().zip(&boxes).any(|(h, b)| (h - b.height).abs() > 0.01) {
            boxes.clear();
            relative_x.clear();
            let mut cursor_y = content_y;
            for ((item_el, item_style), h) in items.iter().zip(item_styles).zip(&heights) {
                let laid_out = layout_element_sized(*item_el, content_x, cursor_y, content_width, None, Some(*h), font, style_cache, intrinsic, image_cache, reuse, depth);
                let (rel_x, rel_y) = crate::box_model::relative_offset(item_style, content_width);
                let margin_bottom = item_style.margin.resolve(content_width).bottom;
                cursor_y = laid_out.y - rel_y + laid_out.height + margin_bottom + gap;
                relative_x.push(rel_x);
                boxes.push(laid_out);
            }
            natural_total_height = (cursor_y - gap - content_y).max(0.0);
        }
    }

    // Cross-axis (width) alignment for non-stretch items.
    match container_style.align_items {
        AlignItems::FlexStart | AlignItems::Stretch => {}
        AlignItems::Center => {
            for (b, rel_x) in boxes.iter_mut().zip(&relative_x) {
                let dx = content_x + (content_width - b.width) / 2.0 + rel_x - b.x;
                translate(b, dx, 0.0);
            }
        }
        AlignItems::FlexEnd => {
            for (b, rel_x) in boxes.iter_mut().zip(&relative_x) {
                let dx = content_x + content_width - b.width + rel_x - b.x;
                translate(b, dx, 0.0);
            }
        }
    }

    // Main-axis (vertical) justify-content — only has anything to
    // distribute when the container has an EXPLICIT height (an
    // auto-height column flex container has no leftover main-axis
    // space by definition, matching real CSS).
    if let Some(explicit_h) = explicit_content_height {
        let leftover = (explicit_h - natural_total_height).max(0.0);
        if leftover > 0.0 && n > 0 {
            let (start_offset, extra_gap) = match container_style.justify_content {
                JustifyContent::FlexStart => (0.0, 0.0),
                JustifyContent::Center => (leftover / 2.0, 0.0),
                JustifyContent::FlexEnd => (leftover, 0.0),
                JustifyContent::SpaceBetween if n > 1 => (0.0, leftover / (n - 1) as f32),
                JustifyContent::SpaceBetween => (0.0, 0.0),
            };
            let mut extra = start_offset;
            for b in &mut boxes {
                translate(b, 0.0, extra);
                extra += extra_gap;
            }
        }
    }

    let used_height = explicit_content_height.unwrap_or(natural_total_height);
    (boxes, used_height)
}

#[cfg(test)]
mod tests {
    use crate::*;
    use css_parser::Stylesheet;
    use dom::{Element, Node};
    use images::ImageCache;
    use text::Font;

    fn test_font() -> Font {
        Font::dejavu_sans()
    }



    fn no_sheet() -> Stylesheet {
        Stylesheet::default()
    }



    // ---- Milestone 10: flexbox ----

    fn flex_container(extra_style: &str) -> Element {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), format!("display:flex;width:600px;{extra_style}")));
        el
    }



    fn child_with_width(w: f32) -> Element {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), format!("width:{w}px;height:20px")));
        el
    }



    #[test]
    fn row_children_are_placed_left_to_right() {
        let mut parent = flex_container("");
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 3);
        assert_eq!(laid_out.children[0].x, 0.0);
        assert_eq!(laid_out.children[1].x, 100.0);
        assert_eq!(laid_out.children[2].x, 200.0);
        // All on the same row (y unchanged across items).
        assert_eq!(laid_out.children[0].y, laid_out.children[1].y);
    }


    #[test]
    fn gap_adds_space_between_row_items() {
        let mut parent = flex_container("gap:10px");
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[1].x, 110.0);
    }


    #[test]
    fn justify_content_center_centers_the_row() {
        let mut parent = flex_container("justify-content:center");
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));
        // total item width 200 in a 600px container -> 400px leftover -> 200px each side

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].x, 200.0);
        assert_eq!(laid_out.children[1].x, 300.0);
    }


    #[test]
    fn justify_content_flex_end_packs_items_at_the_end() {
        let mut parent = flex_container("justify-content:flex-end");
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));
        // 600 - 200 = 400 leftover, all before the items

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].x, 400.0);
        assert_eq!(laid_out.children[1].x, 500.0);
    }


    #[test]
    fn justify_content_space_between_distributes_leftover_evenly() {
        let mut parent = flex_container("justify-content:space-between");
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));
        parent.children.push(Node::Element(child_with_width(100.0)));
        // 600 - 300 = 300 leftover / 2 gaps = 150 each

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].x, 0.0);
        assert_eq!(laid_out.children[1].x, 250.0);
        assert_eq!(laid_out.children[2].x, 500.0);
    }


    #[test]
    fn flex_grow_distributes_leftover_space_by_weight() {
        let mut parent = flex_container("");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "width:100px;height:20px;flex-grow:1".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "width:100px;height:20px;flex-grow:2".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));
        // base total 200, leftover 400, grow weights 1:2 -> a gets +133.33, b gets +266.67

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let width_a = laid_out.children[0].width;
        let width_b = laid_out.children[1].width;
        assert!((width_a - 233.33).abs() < 0.1);
        assert!((width_b - 366.67).abs() < 0.1);
        // b grew roughly twice as much as a, proportional to weight.
        assert!(((width_b - 100.0) / (width_a - 100.0) - 2.0).abs() < 0.01);
    }


    #[test]
    fn flex_shrink_reduces_items_proportionally_when_over_budget() {
        let mut parent = flex_container("");
        parent.children.push(Node::Element(child_with_width(400.0)));
        parent.children.push(Node::Element(child_with_width(400.0)));
        // base total 800 > 600px container -> must shrink by 200 total, equal shrink factor -> equal reduction

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let width_a = laid_out.children[0].width;
        let width_b = laid_out.children[1].width;
        assert!((width_a - 300.0).abs() < 0.1);
        assert!((width_b - 300.0).abs() < 0.1);
    }


    #[test]
    fn flex_basis_overrides_width_as_the_base_size() {
        let mut parent = flex_container("");
        let mut item = Element::new("div");
        item.attributes.push(("style".into(), "width:999px;flex-basis:50px;height:20px".into()));
        parent.children.push(Node::Element(item));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].width, 50.0);
    }


    #[test]
    fn align_items_stretch_is_the_default_and_fills_cross_axis() {
        let mut parent = flex_container("height:200px");
        let mut auto_height = Element::new("div");
        auto_height.set_attr("style", "width:100px");
        parent.children.push(Node::Element(auto_height));
        parent.children.push(Node::Element(child_with_width(100.0))); // height: 20px

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // Container content height is 200px (explicit); stretch makes an
        // auto-height item that tall. Milestone 33: an item with a height
        // of its own keeps it (stretch applies only to an `auto` cross
        // size — CSS Flexbox §9.4 step 11; this test used to assert the
        // opposite).
        assert_eq!(laid_out.children[0].height, 200.0);
        assert_eq!(laid_out.children[1].height, 20.0);
    }


    #[test]
    fn align_items_center_centers_on_cross_axis() {
        let mut parent = flex_container("height:200px;align-items:center");
        parent.children.push(Node::Element(child_with_width(100.0))); // natural height 20px

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].height, 20.0); // did NOT stretch
        assert_eq!(laid_out.children[0].y, laid_out.y + (200.0 - 20.0) / 2.0);
    }


    #[test]
    fn align_items_flex_end_sits_at_the_cross_axis_end() {
        let mut parent = flex_container("height:200px;align-items:flex-end");
        parent.children.push(Node::Element(child_with_width(100.0)));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].y, laid_out.y + 200.0 - 20.0);
    }


    #[test]
    fn column_direction_stacks_vertically() {
        let mut parent = flex_container("flex-direction:column");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "height:30px".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "height:40px".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].y, laid_out.y);
        assert_eq!(laid_out.children[1].y, laid_out.y + 30.0);
    }


    #[test]
    fn column_direction_gap_adds_vertical_space() {
        let mut parent = flex_container("flex-direction:column;gap:15px");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "height:30px".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "height:40px".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[1].y, laid_out.y + 30.0 + 15.0);
    }


    #[test]
    fn column_direction_items_default_to_full_width_stretch() {
        let mut parent = flex_container("flex-direction:column");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "height:30px".into())); // no explicit width
        parent.children.push(Node::Element(a));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // Container content width is 600px (explicit on flex_container); item should fill it.
        assert_eq!(laid_out.children[0].width, 600.0);
    }


    #[test]
    fn column_direction_align_items_center_centers_on_width() {
        let mut parent = flex_container("flex-direction:column;align-items:center");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "width:100px;height:30px".into()));
        parent.children.push(Node::Element(a));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].x, laid_out.x + (600.0 - 100.0) / 2.0);
    }


    #[test]
    fn nested_flex_containers_both_apply_their_own_flex_layout() {
        let mut outer = flex_container("");
        let mut inner = Element::new("div");
        inner.attributes.push(("style".into(), "display:flex;width:200px".into()));
        inner.children.push(Node::Element(child_with_width(50.0)));
        inner.children.push(Node::Element(child_with_width(50.0)));
        outer.children.push(Node::Element(inner));

        let laid_out = layout_document(&outer, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let inner_box = &laid_out.children[0];
        assert_eq!(inner_box.children.len(), 2);
        assert_eq!(inner_box.children[0].x, inner_box.x);
        assert_eq!(inner_box.children[1].x, inner_box.x + 50.0);
    }


    #[test]
    fn empty_flex_container_does_not_panic() {
        let parent = flex_container("");
        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 0);
    }


    #[test]
    fn text_directly_inside_flex_container_is_skipped_not_a_panic() {
        let mut parent = flex_container("");
        parent.children.push(Node::Text("stray text".to_string()));
        parent.children.push(Node::Element(child_with_width(100.0)));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // Only the real element becomes a flex item; the stray text is dropped.
        assert_eq!(laid_out.children.len(), 1);
    }


    #[test]
    fn flex_shorthand_one_produces_equal_growth() {
        let mut parent = flex_container("");
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "height:20px;flex:1".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "height:20px;flex:1".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));
        // Both flex:1 with flex-basis:0 from the shorthand -> split 600px evenly.

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert!((laid_out.children[0].width - 300.0).abs() < 0.1);
        assert!((laid_out.children[1].width - 300.0).abs() < 0.1);
    }


    #[test]
    fn display_block_is_unaffected_by_flex_properties() {
        // A block-display element with flex-direction/justify-content
        // set (meaningless without display:flex) must still lay out
        // as ordinary block stacking.
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "justify-content:center".into()));
        let mut a = Element::new("div");
        a.attributes.push(("style".into(), "height:20px".into()));
        let mut b = Element::new("div");
        b.attributes.push(("style".into(), "height:20px".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));

        let laid_out = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children[0].y, laid_out.y);
        assert_eq!(laid_out.children[1].y, laid_out.y + 20.0); // normal block stacking, not centered
    }


    // ---- Phase D: flexbox completeness + intrinsic sizing ----

    #[test]
    fn flex_order_reorders_items_visually() {
        let mut parent = flex_container("");
        let mut a = Element::new("div");
        a.attributes.push(("id".into(), "a".into()));
        a.attributes.push(("style".into(), "width:50px;height:20px;order:2".into()));
        let mut b = Element::new("div");
        b.attributes.push(("id".into(), "b".into()));
        b.attributes.push(("style".into(), "width:50px;height:20px;order:1".into()));
        parent.children.push(Node::Element(a));
        parent.children.push(Node::Element(b));

        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // b (order:1) comes first visually, so its box is first and leftmost.
        let b_key = match &parent.children[1] {
            Node::Element(e) => e.node_id,
            _ => unreachable!(),
        };
        assert_eq!(laid.children[0].element_key, Some(b_key));
        assert!(laid.children[0].x < laid.children[1].x);
    }


    #[test]
    fn flex_wrap_breaks_items_onto_multiple_lines() {
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "display:flex;flex-wrap:wrap;width:250px".into()));
        for _ in 0..4 {
            parent.children.push(Node::Element(child_with_width(100.0)));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // 4x100px in a 250px container => 2 per line => 2 distinct y values.
        let ys: std::collections::BTreeSet<i32> = laid.children.iter().map(|c| c.y as i32).collect();
        assert_eq!(ys.len(), 2, "expected two flex lines");
        // Container grew to hold both lines.
        assert!(laid.height >= 40.0);
    }


    #[test]
    fn flex_nowrap_keeps_one_line_even_when_overflowing() {
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "display:flex;width:250px".into()));
        for _ in 0..4 {
            parent.children.push(Node::Element(child_with_width(100.0)));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let ys: std::collections::BTreeSet<i32> = laid.children.iter().map(|c| c.y as i32).collect();
        assert_eq!(ys.len(), 1);
    }


    #[test]
    fn align_content_center_groups_wrapped_lines_in_the_middle() {
        let mut parent = Element::new("div");
        parent
            .attributes
            .push(("style".into(), "display:flex;flex-wrap:wrap;width:250px;height:200px;align-content:center".into()));
        for _ in 0..4 {
            parent.children.push(Node::Element(child_with_width(100.0)));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let min_y = laid.children.iter().map(|c| c.y).fold(f32::MAX, f32::min);
        // Two 20px lines + gap in a 200px box, centered => well below the top.
        assert!(min_y > laid.y + 50.0, "lines should be pushed toward the vertical center, min_y={min_y}");
    }


    // ---- Milestone 33: flexible lengths, margins, min/max ----

    fn row(items: &[&str], container: &str) -> LayoutBox {
        let mut parent = flex_container(container);
        for style in items {
            let mut item = Element::new("div");
            item.set_attr("style", *style);
            item.children.push(Node::Text("word".into()));
            parent.children.push(Node::Element(item));
        }
        layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new())
    }

    #[test]
    fn an_items_contents_move_with_it() {
        let laid = row(&["width: 100px", "width: 100px"], "");
        for item in &laid.children {
            let text = &item.children[0];
            assert!(text.x >= item.x && text.x < item.x + item.width, "text at {} outside its item at {}..{}", text.x, item.x, item.x + item.width);
        }
    }

    #[test]
    fn auto_margins_absorb_free_space() {
        let laid = row(&["width: 100px", "width: 100px; margin-left: auto"], "");
        assert_eq!(laid.children[1].x, 500.0, "pushed to the end of the 600px line");
        let laid = row(&["width: 100px; margin: 0 auto"], "justify-content: flex-end");
        assert_eq!(laid.children[0].x, 250.0, "centred; justify-content has nothing left to place");
        let laid = row(&["width: 100px; margin-left: 20px", "width: 100px; margin-right: 30px"], "");
        assert_eq!((laid.children[0].x, laid.children[1].x), (20.0, 120.0), "margins take space");
    }

    #[test]
    fn growing_items_freeze_at_their_max_and_the_rest_take_the_space() {
        let laid = row(&["flex-grow: 1; flex-basis: 0; max-width: 100px", "flex-grow: 1; flex-basis: 0"], "");
        assert_eq!(laid.children[0].width, 100.0);
        assert_eq!(laid.children[1].width, 500.0);
    }

    #[test]
    fn items_do_not_shrink_below_their_min_content_unless_told() {
        let long = "width: 500px; flex-shrink: 1";
        let mut parent = flex_container("");
        for _ in 0..2 {
            let mut item = Element::new("div");
            item.set_attr("style", long);
            item.children.push(Node::Text("unbreakablewordthatislong".into()));
            parent.children.push(Node::Element(item));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        let word = test_font().measure("unbreakablewordthatislong", 16.0).width;
        assert!(laid.children[0].width >= word - 0.01, "automatic minimum: {} < {word}", laid.children[0].width);
        let mut parent = flex_container("");
        for _ in 0..2 {
            let mut item = Element::new("div");
            item.set_attr("style", "width: 500px; min-width: 0");
            item.children.push(Node::Text("unbreakablewordthatislong".into()));
            parent.children.push(Node::Element(item));
        }
        let laid = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid.children[0].width, 300.0, "min-width: 0 lets it shrink");
    }

    #[test]
    fn column_items_grow_and_shrink_in_a_definite_height() {
        let column = |container: &str, items: &[&str]| {
            let mut parent = Element::new("div");
            parent.set_attr("style", format!("display: flex; flex-direction: column; width: 100px; {container}"));
            for st in items {
                let mut item = Element::new("div");
                item.set_attr("style", *st);
                parent.children.push(Node::Element(item));
            }
            layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new())
        };
        let l = column("height: 300px", &["height: 50px", "flex-grow: 1", "flex-grow: 2"]);
        let hs: Vec<f32> = l.children.iter().map(|c| c.height).collect();
        assert_eq!(hs, vec![50.0, 250.0 / 3.0, 500.0 / 3.0]);
        assert_eq!(l.children[2].y, 50.0 + 250.0 / 3.0, "restacked");
        let l = column("height: 100px", &["height: 80px", "height: 80px; min-height: 0", "flex: 1"]);
        let total: f32 = l.children.iter().map(|c| c.height).sum();
        assert!((total - 100.0).abs() < 0.01, "shrunk to fit: {total}");
        let l = column("", &["height: 50px", "flex-grow: 1"]);
        assert_eq!(l.children[1].height, 0.0, "an auto-height column has no free space to grow into");
    }
}
