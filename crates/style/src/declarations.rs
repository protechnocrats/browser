//! Applying ONE already-cascade-resolved `property: value` declaration
//! onto a `Style` — the giant per-property match plus every shorthand/
//! value parser (`border`, `flex`, `transform`, `background-position`,
//! ...) it delegates to. No selector matching or cascade-priority
//! logic lives here (see `crate::cascade` for that); by the time
//! anything here runs, the cascade has already decided which
//! declaration wins.

use crate::color::parse_color;
use crate::length::{parse_length, LengthContext, ViewportSize};
use crate::style::Style;
use crate::transitions::{parse_duration_secs, parse_transition_shorthand};
use crate::values::{
    AlignContent, AlignItems, BackgroundPosition, BackgroundRepeat, BackgroundSize, BoxShadow, Clear, Float, Overflow, Position, Visibility, BorderSpec, BorderStyle, BoxSizing, Color, Display, Edges,
    FlexDirection, FlexWrap, IntrinsicKeyword, JustifyContent, Length, ObjectFit, TextAlign, Transform, VerticalAlign,
    WhiteSpace, MAX_FONT_SIZE_PX,
};

/// Applies one property/value pair onto `style`, called once per
/// declaration per element per restyle — real per-frame traffic on any
/// styled page. Used to build a throwaway `Declaration` (two fresh
/// `String` allocations via `.to_string()`) purely so the match body
/// below could read `decl.value`; `property`/`value` are already `&str`
/// parameters, so that struct was pure waste, unconditionally, on
/// every single call.
///
/// Milestone 33: lengths resolve against `style`'s font size (so the
/// cascade applies `font-size` first) and root font size, and `viewport`.
pub(crate) fn apply_declaration(style: &mut Style, property: &str, value: &str, viewport: ViewportSize) {
    let ctx = LengthContext {
        font_size: style.font_size,
        root_font_size: style.root_font_size,
        viewport_width: viewport.width,
        viewport_height: viewport.height,
    };
    // A non-negative length-percentage (sizes, padding); pixels only
    // (properties with no percentage basis here).
    // Negative sizes are invalid; a `calc()` mixing signs
    // (`calc(100% - 20px)`) is clamped at zero by layout instead.
    let size = |v: &str| parse_length(v, &ctx).filter(|l| l.px >= 0.0 || l.percent > 0.0).filter(|l| l.percent >= 0.0 || l.px > 0.0);
    let px = |v: &str| parse_length(v, &ctx).and_then(Length::absolute);
    match property {
        "width" | "height" => {
            let keyword = parse_intrinsic_keyword(value);
            let parsed = if keyword.is_some() || value.trim().eq_ignore_ascii_case("auto") {
                Some(None)
            } else {
                size(value).map(Some)
            };
            // An invalid value leaves the previous declaration in force.
            if let Some(parsed) = parsed {
                if property == "width" {
                    style.width_intrinsic = keyword;
                    style.width = parsed;
                } else {
                    style.height_intrinsic = keyword;
                    style.height = parsed;
                }
            }
        }
        "min-width" | "min-height" | "max-width" | "max-height" => {
            let v = value.trim().to_ascii_lowercase();
            // `auto` (min) and `none` (max) are "no constraint"; the
            // intrinsic keywords are not modelled as constraints yet.
            let parsed = if v == "auto" || v == "none" {
                Some(None)
            } else if parse_intrinsic_keyword(&v).is_some() {
                diagnostics::unsupported::record("css-value", &format!("{property}: {v}"));
                None
            } else {
                size(&v).map(Some)
            };
            if let Some(parsed) = parsed {
                match property {
                    "min-width" => style.limits_mut().min_width = parsed,
                    "min-height" => style.limits_mut().min_height = parsed,
                    "max-width" => style.limits_mut().max_width = parsed,
                    _ => style.limits_mut().max_height = parsed,
                }
            }
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            let keyword = |t: &str| match t.to_ascii_lowercase().as_str() {
                "visible" => Some(Overflow::Visible),
                "hidden" => Some(Overflow::Hidden),
                "clip" => Some(Overflow::Clip),
                "scroll" => Some(Overflow::Scroll),
                "auto" | "overlay" => Some(Overflow::Auto),
                _ => None,
            };
            let values: Option<Vec<Overflow>> = value.split_whitespace().map(keyword).collect();
            match (property, values.as_deref()) {
                ("overflow", Some([both])) => (style.overflow_x, style.overflow_y) = (*both, *both),
                ("overflow", Some([x, y])) => (style.overflow_x, style.overflow_y) = (*x, *y),
                ("overflow-x", Some([x])) => style.overflow_x = *x,
                ("overflow-y", Some([y])) => style.overflow_y = *y,
                _ => {}
            }
            // CSS Overflow 3 §3.1: if one axis is visible/clip and the
            // other is not, visible becomes auto and clip becomes hidden.
            let (x, y) = (style.overflow_x, style.overflow_y);
            let scrollish = |o: Overflow| matches!(o, Overflow::Hidden | Overflow::Scroll | Overflow::Auto);
            if scrollish(x) || scrollish(y) {
                let fix = |o: Overflow| match o {
                    Overflow::Visible => Overflow::Auto,
                    Overflow::Clip => Overflow::Hidden,
                    o => o,
                };
                style.overflow_x = fix(x);
                style.overflow_y = fix(y);
            }
        }
        "box-shadow" => {
            if let Some(shadows) = parse_box_shadow(value, &ctx, style.color) {
                style.box_shadow = shadows;
            }
        }
        "border-collapse" => match value.trim().to_ascii_lowercase().as_str() {
            "collapse" => style.table_borders.collapse = true,
            "separate" => style.table_borders.collapse = false,
            _ => {}
        },
        "border-spacing" => {
            let parts: Option<Vec<f32>> = split_values(value).into_iter().map(|t| px(t).filter(|v| *v >= 0.0)).collect();
            match parts.as_deref() {
                Some([both]) => style.table_borders.spacing = (*both, *both),
                Some([h, v]) => style.table_borders.spacing = (*h, *v),
                _ => {}
            }
        }
        "float" => {
            style.float = match value.trim().to_ascii_lowercase().as_str() {
                "left" | "inline-start" => Float::Left,
                "right" | "inline-end" => Float::Right,
                "none" => Float::None,
                _ => style.float,
            };
        }
        "clear" => {
            style.clear = match value.trim().to_ascii_lowercase().as_str() {
                "none" => Clear { left: false, right: false },
                "left" | "inline-start" => Clear { left: true, right: false },
                "right" | "inline-end" => Clear { left: false, right: true },
                "both" => Clear { left: true, right: true },
                _ => style.clear,
            };
        }
        "opacity" => {
            let v = value.trim();
            let parsed = match v.strip_suffix('%') {
                Some(p) => p.trim().parse::<f32>().ok().map(|p| p / 100.0),
                None => v.parse::<f32>().ok(),
            };
            if let Some(o) = parsed.filter(|o| o.is_finite()) {
                style.opacity = o.clamp(0.0, 1.0);
            }
        }
        "visibility" => {
            style.visibility = match value.trim().to_ascii_lowercase().as_str() {
                "visible" => Visibility::Visible,
                "hidden" | "collapse" => Visibility::Hidden,
                _ => style.visibility,
            };
        }
        "position" => {
            style.position = match value.trim().to_ascii_lowercase().as_str() {
                "static" => Position::Static,
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                "sticky" | "-webkit-sticky" => Position::Sticky,
                _ => style.position,
            };
        }
        "top" | "right" | "bottom" | "left" => {
            if let Some(v) = margin_value(value, &ctx) {
                match property {
                    "top" => style.inset_mut().top = v,
                    "right" => style.inset_mut().right = v,
                    "bottom" => style.inset_mut().bottom = v,
                    _ => style.inset_mut().left = v,
                }
            }
        }
        "inset" => {
            if let Some(edges) = parse_edge_shorthand(value, |t| margin_value(t, &ctx)) {
                *style.inset_mut() = edges;
            }
        }
        "z-index" => {
            let v = value.trim();
            if v.eq_ignore_ascii_case("auto") {
                style.z_index = None;
            } else if let Ok(z) = v.parse::<i32>() {
                style.z_index = Some(z);
            }
        }
        "background-color" => {
            if let Some(c) = parse_color(value, style.color) {
                style.background_color = c;
            }
        }
        "margin" => {
            if let Some(edges) = parse_edge_shorthand(value, |t| margin_value(t, &ctx)) {
                style.margin = edges;
            }
        }
        "margin-top" => {
            if let Some(m) = margin_value(value, &ctx) {
                style.margin.top = m;
            }
        }
        "margin-right" => {
            if let Some(m) = margin_value(value, &ctx) {
                style.margin.right = m;
            }
        }
        "margin-bottom" => {
            if let Some(m) = margin_value(value, &ctx) {
                style.margin.bottom = m;
            }
        }
        "margin-left" => {
            if let Some(m) = margin_value(value, &ctx) {
                style.margin.left = m;
            }
        }
        "padding" => {
            if let Some(edges) = parse_edge_shorthand(value, size) {
                style.padding = edges;
            }
        }
        "padding-top" => {
            if let Some(l) = size(value) {
                style.padding.top = l;
            }
        }
        "padding-right" => {
            if let Some(l) = size(value) {
                style.padding.right = l;
            }
        }
        "padding-bottom" => {
            if let Some(l) = size(value) {
                style.padding.bottom = l;
            }
        }
        "padding-left" => {
            if let Some(l) = size(value) {
                style.padding.left = l;
            }
        }
        // M6's gap: the box forms (one to four values, top right bottom
        // left) and every side's own properties.
        "border-width" => {
            if let Some(w) = box_values(value, |v| border_width_value(v, &ctx)) {
                set_sides(style.border_mut(), w, |s, v| s.width = v);
            }
        }
        "border-color" => {
            let current = style.color;
            if let Some(c) = box_values(value, |v| parse_color(v, current)) {
                set_sides(style.border_mut(), c, |s, v| s.color = Some(v));
            }
        }
        "border-style" => {
            if let Some(st) = box_values(value, parse_border_style) {
                set_sides(style.border_mut(), st, |s, v| s.style = v);
            }
        }
        "border" => {
            // The shorthand resets what it omits (CSS Backgrounds §4.4).
            let side = parse_border_side(value, style.color, &ctx);
            *style.border_mut() = Edges::all(side);
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = parse_border_side(value, style.color, &ctx);
            *border_side_mut(style.border_mut(), &property[7..]) = side;
        }
        "border-top-width" | "border-right-width" | "border-bottom-width" | "border-left-width" => {
            if let Some(w) = border_width_value(value, &ctx) {
                border_side_mut(style.border_mut(), side_name(property)).width = w;
            }
        }
        "border-top-style" | "border-right-style" | "border-bottom-style" | "border-left-style" => {
            if let Some(st) = parse_border_style(value.trim()) {
                border_side_mut(style.border_mut(), side_name(property)).style = st;
            }
        }
        "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color" => {
            if let Some(c) = parse_color(value, style.color) {
                border_side_mut(style.border_mut(), side_name(property)).color = Some(c);
            }
        }
        "border-radius" => {
            // Phase C: one length only (uniform corners). The
            // multi-value / per-corner / elliptical / `%` forms are a
            // further extension — documented.
            if let Some(r) = split_values(value).first().and_then(|v| px(v)) {
                style.border_radius = r.max(0.0);
            }
        }
        "outline-width" => {
            if let Some(w) = border_width_value(value, &ctx) {
                style.outline_width = w;
            }
        }
        "outline-color" => {
            if let Some(c) = parse_color(value, style.color) {
                style.outline_color = c;
            }
        }
        "outline-style" => {
            if let Some(s) = parse_border_style(value.trim()) {
                style.outline_style = s;
            }
        }
        "outline-offset" => {
            if let Some(o) = px(value) {
                style.outline_offset = o;
            }
        }
        "outline" => parse_outline_shorthand(value, style, &ctx),
        "box-sizing" => {
            style.box_sizing = match value.trim() {
                "border-box" => BoxSizing::BorderBox,
                "content-box" => BoxSizing::ContentBox,
                _ => style.box_sizing,
            };
        }
        "font-size" => apply_font_size(style, value, &ctx),
        "font-family" | "font-weight" | "font-style" | "line-height" | "text-decoration" | "text-decoration-line" | "text-decoration-color"
        | "text-transform" | "direction" | "unicode-bidi" | "font" => apply_text_property(style, property, value, &ctx),
        "color" => {
            if let Some(c) = parse_color(value, style.color) {
                style.color = c;
            }
        }
        "text-align" => {
            style.text_align = match value.trim() {
                "left" => TextAlign::Left,
                "start" => TextAlign::Start,
                "end" => TextAlign::End,
                "center" => TextAlign::Center,
                "right" => TextAlign::Right,
                "justify" => TextAlign::Justify,
                _ => style.text_align,
            };
        }
        "white-space" => {
            style.white_space = match value.trim() {
                "normal" => WhiteSpace::Normal,
                "nowrap" => WhiteSpace::Nowrap,
                "pre" => WhiteSpace::Pre,
                "pre-wrap" => WhiteSpace::PreWrap,
                _ => style.white_space,
            };
        }
        "vertical-align" => {
            style.vertical_align = match value.trim() {
                "baseline" => VerticalAlign::Baseline,
                "top" => VerticalAlign::Top,
                "middle" => VerticalAlign::Middle,
                "bottom" => VerticalAlign::Bottom,
                "text-top" => VerticalAlign::TextTop,
                "text-bottom" => VerticalAlign::TextBottom,
                "sub" => VerticalAlign::Sub,
                "super" => VerticalAlign::Super,
                _ => style.vertical_align,
            };
        }
        "content" => {
            // Kept as written; resolved for a pseudo-element at the end of
            // its cascade (`crate::content::resolve_content`).
            style.content = Some(value.trim().to_string());
        }
        "background-image" => {
            style.background_image_path = parse_background_image(value);
        }
        "background-repeat" => {
            if let Some(r) = parse_background_repeat(value) {
                style.background_repeat = r;
            }
        }
        "background" => parse_background_shorthand(value, style, &ctx),
        "display" => {
            style.display = match value.trim().to_ascii_lowercase().as_str() {
                "flex" => Display::Flex,
                "block" | "flow-root" => Display::Block,
                "none" => Display::None,
                "inline" => Display::Inline,
                "inline-block" => Display::InlineBlock,
                "inline-flex" => Display::InlineFlex,
                "table" => Display::Table,
                "inline-table" => Display::InlineTable,
                "table-row-group" | "table-header-group" | "table-footer-group" => Display::TableRowGroup,
                "table-row" => Display::TableRow,
                "table-cell" => Display::TableCell,
                "table-caption" => Display::TableCaption,
                "table-column" | "table-column-group" => Display::TableColumn,
                "grid" => Display::Grid,
                "inline-grid" => Display::InlineGrid,
                // A block with a marker; markers are not drawn yet.
                "list-item" => Display::Block,
                other => {
                    diagnostics::unsupported::record("css-value", &format!("display: {other}"));
                    style.display
                }
            };
        }
        "flex-direction" => {
            style.flex_direction = match value.trim() {
                "row" => FlexDirection::Row,
                "column" => FlexDirection::Column,
                _ => style.flex_direction,
            };
        }
        "justify-content" => {
            style.justify_content = match value.trim() {
                "flex-start" => JustifyContent::FlexStart,
                "center" => JustifyContent::Center,
                "flex-end" => JustifyContent::FlexEnd,
                "space-between" => JustifyContent::SpaceBetween,
                _ => style.justify_content,
            };
        }
        "align-items" => {
            style.align_items = match value.trim() {
                "flex-start" => AlignItems::FlexStart,
                "center" => AlignItems::Center,
                "flex-end" => AlignItems::FlexEnd,
                "stretch" => AlignItems::Stretch,
                _ => style.align_items,
            };
        }
        // `gap: <row> [<column>]`; `gap` is the column gap, `row_gap` the
        // row gap (`grid-gap` and friends are the legacy names).
        "gap" | "grid-gap" => {
            let parts: Option<Vec<f32>> = split_values(value).into_iter().map(|v| px(v).filter(|g| *g >= 0.0)).collect();
            match parts.as_deref() {
                Some([both]) => (style.row_gap, style.gap) = (*both, *both),
                Some([row, column]) => (style.row_gap, style.gap) = (*row, *column),
                _ => {}
            }
        }
        "row-gap" | "grid-row-gap" => {
            if let Some(g) = px(value).filter(|g| *g >= 0.0) {
                style.row_gap = g;
            }
        }
        "column-gap" | "grid-column-gap" => {
            if let Some(g) = px(value).filter(|g| *g >= 0.0) {
                style.gap = g;
            }
        }
        "grid-template-columns" | "grid-template-rows" => {
            if let Some(list) = crate::grid::parse_track_list(value, &ctx) {
                if property == "grid-template-columns" {
                    style.grid_mut().template_columns = list;
                } else {
                    style.grid_mut().template_rows = list;
                }
            }
        }
        "grid-template-areas" => {
            if let Some(areas) = crate::grid::parse_template_areas(value) {
                style.grid_mut().template_areas = areas;
            }
        }
        "grid-auto-columns" | "grid-auto-rows" => {
            if let Some(t) = crate::grid::parse_auto_track(value, &ctx) {
                if property == "grid-auto-columns" {
                    style.grid_mut().auto_columns = t;
                } else {
                    style.grid_mut().auto_rows = t;
                }
            }
        }
        "grid-column" | "grid-row" => {
            if let Some(p) = crate::grid::parse_placement(value) {
                if property == "grid-column" {
                    style.grid_mut().column = p;
                } else {
                    style.grid_mut().row = p;
                }
            }
        }
        "grid-column-start" | "grid-column-end" | "grid-row-start" | "grid-row-end" => {
            if let Some(p) = crate::grid::parse_placement(value) {
                let grid = style.grid_mut();
                let target = if property.starts_with("grid-column") { &mut grid.column } else { &mut grid.row };
                if property.ends_with("start") {
                    target.start = p.start;
                } else {
                    target.end = p.start;
                }
            }
        }
        "grid-area" => {
            if let Some((row, column)) = crate::grid::parse_grid_area(value) {
                let grid = style.grid_mut();
                (grid.row, grid.column) = (row, column);
            }
        }
        "flex-grow" => {
            if let Ok(v) = value.trim().parse::<f32>() {
                style.flex_grow = v;
            }
        }
        "flex-shrink" => {
            if let Ok(v) = value.trim().parse::<f32>() {
                style.flex_shrink = v;
            }
        }
        "flex-basis" => {
            if value.trim() == "auto" || value.trim() == "content" {
                style.flex_basis = None;
            } else if let Some(l) = size(value) {
                style.flex_basis = Some(l);
            }
        }
        "flex" => parse_flex_shorthand(value, style),
        "flex-wrap" => {
            style.flex_wrap = match value.trim() {
                "wrap" => FlexWrap::Wrap,
                "nowrap" => FlexWrap::Nowrap,
                "wrap-reverse" => FlexWrap::WrapReverse,
                _ => style.flex_wrap,
            };
        }
        "align-content" => {
            style.align_content = match value.trim() {
                "stretch" => AlignContent::Stretch,
                "flex-start" | "start" => AlignContent::FlexStart,
                "center" => AlignContent::Center,
                "flex-end" | "end" => AlignContent::FlexEnd,
                "space-between" => AlignContent::SpaceBetween,
                "space-around" => AlignContent::SpaceAround,
                _ => style.align_content,
            };
        }
        "order" => {
            if let Ok(v) = value.trim().parse::<i32>() {
                style.order = v;
            }
        }
        "object-fit" => {
            style.object_fit = match value.trim() {
                "fill" => ObjectFit::Fill,
                "contain" => ObjectFit::Contain,
                "cover" => ObjectFit::Cover,
                "none" => ObjectFit::None,
                "scale-down" => ObjectFit::ScaleDown,
                _ => style.object_fit,
            };
        }
        "background-size" => {
            if let Some(size) = parse_background_size(value, &ctx) {
                style.background_size = size;
            }
        }
        "background-position" => {
            if let Some(p) = parse_background_position(&split_values(value), &ctx) {
                style.background_position = p;
            }
        }
        "transform" => {
            style.transform = parse_transform(value, &ctx);
        }
        "will-change" => {
            style.will_change_transform = value.split(',').any(|v| v.trim().eq_ignore_ascii_case("transform"));
        }
        "transition" => {
            style.transitions = parse_transition_shorthand(value);
        }
        "animation-name" => {
            let v = value.trim();
            style.animation_name = if v.is_empty() || v == "none" { None } else { Some(v.to_string()) };
        }
        "animation-duration" => {
            if let Some(d) = parse_duration_secs(value) {
                style.animation_duration = d;
            }
        }
        "animation-timing-function" => {
            // One animation per element: its first entry.
            if let Some(e) = value.split(',').next().and_then(crate::transitions::parse_easing) {
                style.animation_timing_function = e;
            }
        }
        "animation-iteration-count" => {
            let v = value.trim();
            if v == "infinite" {
                style.animation_iteration_count = None;
            } else if let Ok(n) = v.parse::<f32>() {
                style.animation_iteration_count = Some(n);
            }
        }
        "animation" => {
            // Shorthand: a duration (a bare number or `<n>s`/`<n>ms`
            // token), `infinite` or an iteration count, an easing (M22's
            // gap: applied since the gap pass; a `cubic-bezier()` must be
            // written without spaces), and the animation name — whichever
            // remaining token isn't one of those or a keyword. A delay
            // token is taken as a second duration and ignored.
            let mut duration_seen = false;
            for token in value.split_whitespace() {
                if let Some(e) = crate::transitions::parse_easing(token) {
                    style.animation_timing_function = e;
                } else if token == "infinite" {
                    style.animation_iteration_count = None;
                } else if let Some(d) = parse_duration_secs(token) {
                    if !duration_seen {
                        style.animation_duration = d;
                        duration_seen = true;
                    }
                } else if let Ok(n) = token.parse::<f32>() {
                    style.animation_iteration_count = Some(n);
                } else if !matches!(token, "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" | "normal" | "forwards" | "backwards" | "both" | "none") {
                    style.animation_name = Some(token.to_string());
                }
            }
        }
        // Unrecognized properties are ignored (as CSS requires), and
        // counted for the conformance report (a no-op unless enabled).
        unknown => {
            if !unknown.starts_with("--") {
                diagnostics::unsupported::record("css-property", unknown);
            }
        }
    }
}

/// `translate(x, y)` / `translateX(x)` / `translateY(y)` /
/// `scale(s)` / `scale(sx, sy)` — a space-separated chain of these
/// composes (real CSS allows this too); anything else in the chain
/// (rotate, skew, matrix, 3D variants) is silently skipped rather
/// than rejecting the whole declaration, same graceful-degradation
/// policy used throughout this project. `translate*` arguments are
/// pixel lengths; `scale` arguments are plain unitless numbers.
fn parse_transform(value: &str, ctx: &LengthContext) -> Transform {
    let mut t = Transform::default();
    let mut remaining = value.trim();

    while let Some(open) = remaining.find('(') {
        let name = remaining[..open].trim();
        let Some(close_rel) = remaining[open..].find(')') else { break };
        let close = open + close_rel;
        let args_str = &remaining[open + 1..close];
        let args: Vec<f32> = args_str
            .split(',')
            .filter_map(|a| {
                let a = a.trim();
                // A percentage translate is relative to the element's own
                // box, which the cascade does not know — not modelled.
                parse_length(a, ctx).and_then(Length::absolute).or_else(|| a.parse::<f32>().ok())
            })
            .collect();
        // `rotate()`/`skew()` take angles (`deg`), not lengths — angles
        // need their own tiny parse here.
        let angle_args: Vec<f32> = args_str.split(',').filter_map(|a| parse_angle_deg(a.trim())).collect();

        match name {
            "rotate" => {
                if let Some(&deg) = angle_args.first() {
                    t.rotate_deg += deg;
                }
            }
            "skew" => {
                if let Some(&x) = angle_args.first() {
                    t.skew_x_deg += x;
                }
                if let Some(&y) = angle_args.get(1) {
                    t.skew_y_deg += y;
                }
            }
            "skewX" => {
                if let Some(&x) = angle_args.first() {
                    t.skew_x_deg += x;
                }
            }
            "skewY" => {
                if let Some(&y) = angle_args.first() {
                    t.skew_y_deg += y;
                }
            }
            "translate" => {
                if let Some(&x) = args.first() {
                    t.translate_x += x;
                }
                if let Some(&y) = args.get(1) {
                    t.translate_y += y;
                }
            }
            "translateX" => {
                if let Some(&x) = args.first() {
                    t.translate_x += x;
                }
            }
            "translateY" => {
                if let Some(&y) = args.first() {
                    t.translate_y += y;
                }
            }
            "scale" => {
                if let Some(&s) = args.first() {
                    let sy = args.get(1).copied().unwrap_or(s);
                    t.scale_x *= s;
                    t.scale_y *= sy;
                }
            }
            _ => {} // matrix()/3D transforms — still out of scope, skipped
        }

        remaining = remaining[close + 1..].trim();
    }

    t
}

/// Milestone 22: `rotate()`/`skew()`'s angle arguments — `deg` (the
/// overwhelming common real-world unit) and bare `turn`/`rad` are NOT
/// supported (a real, narrow scope cut; `deg` covers the common case
/// this codebase's own demo and tests use). Malformed/unrecognized
/// input yields `None`, same graceful-degradation policy as
/// `parse_length`.
fn parse_angle_deg(s: &str) -> Option<f32> {
    s.strip_suffix("deg")?.trim().parse::<f32>().ok()
}

/// `flex: <grow>` (the overwhelmingly common real-world form —
/// `flex: 1`, `flex: 2`, etc. — sets `flex-grow` to that number,
/// `flex-shrink: 1`, `flex-basis: 0`) and `flex: none` (`grow: 0,
/// shrink: 0, basis: auto` — the item neither grows nor shrinks).
/// The full shorthand grammar also allows explicit
/// `<grow> <shrink> <basis>` triples and other combinations; only the
/// two forms above are implemented, a deliberate scope reduction for
/// this milestone (see milestones/milestones-01-17.md) rather
/// than the complete CSS flex-shorthand grammar.
fn parse_flex_shorthand(value: &str, style: &mut Style) {
    let value = value.trim();
    if value == "none" {
        style.flex_grow = 0.0;
        style.flex_shrink = 0.0;
        style.flex_basis = None;
        return;
    }
    if let Ok(grow) = value.parse::<f32>() {
        style.flex_grow = grow;
        style.flex_shrink = 1.0;
        style.flex_basis = Some(Length::ZERO);
    }
}

/// Parses `url(path)`, `url("path")`, or `url('path')` into just the
/// path — `none` (any case) clears an existing value. Anything else
/// unrecognized returns `None` (leaves it unset) rather than treating
/// malformed input as a valid path.
fn parse_url(value: &str) -> Option<String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("none") {
        return None;
    }
    let inner = v.strip_prefix("url(")?.strip_suffix(')')?.trim();
    let unquoted = inner.trim_matches(|c| c == '"' || c == '\'');
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// Standard CSS 1/2/3/4-value edge shorthand (`margin`/`padding`):
/// 1 value = all sides; 2 = vertical, horizontal; 3 = top, horizontal,
/// bottom; 4 = top, right, bottom, left. Returns `None` (leaving the
/// existing value untouched) if any token fails to parse or the count
/// isn't 1-4 — malformed shorthand shouldn't silently zero out a
/// previously-set value.
fn parse_edge_shorthand<T: Copy>(value: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Edges<T>> {
    let tokens: Vec<T> = split_values(value).into_iter().map(parse).collect::<Option<Vec<_>>>()?;
    match tokens.len() {
        1 => Some(Edges::all(tokens[0])),
        2 => Some(Edges { top: tokens[0], bottom: tokens[0], left: tokens[1], right: tokens[1] }),
        3 => Some(Edges { top: tokens[0], left: tokens[1], right: tokens[1], bottom: tokens[2] }),
        4 => Some(Edges { top: tokens[0], right: tokens[1], bottom: tokens[2], left: tokens[3] }),
        _ => None,
    }
}

/// One margin value: a length-percentage, or `auto` (`Some(None)`).
fn margin_value(value: &str, ctx: &LengthContext) -> Option<Option<Length>> {
    if value.trim().eq_ignore_ascii_case("auto") {
        Some(None)
    } else {
        parse_length(value, ctx).map(Some)
    }
}

/// `border-width`/`outline-width`: a non-negative pixel length or one of
/// the keywords (`thin`/`medium`/`thick` are 1/3/5px in every engine).
fn border_width_value(value: &str, ctx: &LengthContext) -> Option<f32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(5.0),
        v => parse_length(v, ctx).and_then(Length::absolute).filter(|w| *w >= 0.0),
    }
}

/// Milestone 33: `font-size`. Relative values (`em`, `%`, `smaller`,
/// `larger`) are relative to the PARENT's font size, which is
/// `ctx.font_size` here: the cascade applies every `font-size`
/// declaration against the inherited size, before anything else.
///
/// Zero-trust: `font-size` is untrusted CSS, and glyph rasterization
/// allocates a bitmap proportional to its square. Non-finite values are
/// rejected by the length parser; a negative size is invalid (ignored,
/// like any other invalid CSS); anything larger than `MAX_FONT_SIZE_PX`
/// is clamped (found by the M25 fuzz harness: these used to abort the
/// process on allocation failure).
pub(crate) fn apply_font_size(style: &mut Style, value: &str, ctx: &LengthContext) {
    let parent = ctx.font_size;
    let size = match value.trim().to_ascii_lowercase().as_str() {
        // The CSS Fonts 4 absolute-size table for a 16px `medium`.
        "xx-small" => Some(9.0),
        "x-small" => Some(10.0),
        "small" => Some(13.0),
        "medium" => Some(16.0),
        "large" => Some(18.0),
        "x-large" => Some(24.0),
        "xx-large" => Some(32.0),
        "xxx-large" => Some(48.0),
        "smaller" => Some(parent / 1.2),
        "larger" => Some(parent * 1.2),
        "inherit" => Some(parent),
        v => crate::length::parse_length_unchecked(v, ctx).map(|l| l.resolve(parent)),
    };
    match size {
        Some(px) if px.is_finite() && px >= 0.0 => {
            if px > MAX_FONT_SIZE_PX {
                diagnostics::guard(diagnostics::Guard::FontSizeClamped, "font-size clamped to the maximum");
            }
            style.font_size = px.min(MAX_FONT_SIZE_PX);
        }
        Some(_) => diagnostics::guard(diagnostics::Guard::FontSizeClamped, "non-finite or negative font-size ignored"),
        None => {}
    }
}

/// Splits a value into its space-separated components, keeping anything
/// inside parentheses together (`calc(100% - 1em) auto` is two values).
pub(crate) fn split_values(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0usize, None);
    for (i, c) in value.char_indices() {
        match c {
            '(' => {
                depth += 1;
                start.get_or_insert(i);
            }
            ')' => depth = depth.saturating_sub(1),
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

fn parse_intrinsic_keyword(value: &str) -> Option<IntrinsicKeyword> {
    match value.trim() {
        "min-content" => Some(IntrinsicKeyword::MinContent),
        "max-content" => Some(IntrinsicKeyword::MaxContent),
        "fit-content" => Some(IntrinsicKeyword::FitContent),
        _ => None,
    }
}

/// `box-shadow`: `none`, or a comma-separated list of `[inset] <x> <y>
/// [<blur> [<spread>]] [<color>]` in any order of the keyword and colour
/// (CSS Backgrounds 3 §7.1). `None` if invalid. Bounded: at most
/// [`crate::MAX_BOX_SHADOWS`] shadows, blur/spread clamped.
fn parse_box_shadow(value: &str, ctx: &LengthContext, current: crate::values::Color) -> Option<Vec<BoxShadow>> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let layers = split_layers(value);
    if layers.len() > crate::values::MAX_BOX_SHADOWS {
        return None;
    }
    let mut out = Vec::new();
    for layer in layers {
        let mut lengths = Vec::new();
        let mut color = None;
        let mut inset = false;
        for token in split_values(layer) {
            if token.eq_ignore_ascii_case("inset") && !inset {
                inset = true;
            } else if let Some(l) = parse_length(token, ctx).and_then(Length::absolute) {
                lengths.push(l);
            } else {
                // One colour at most; anything else makes it invalid.
                color = Some(parse_color(token, current).filter(|_| color.is_none())?);
            }
        }
        let extent = crate::values::MAX_SHADOW_EXTENT;
        let (offset_x, offset_y, blur, spread) = match lengths[..] {
            [x, y] => (x, y, 0.0, 0.0),
            [x, y, b] => (x, y, b, 0.0),
            [x, y, b, s] => (x, y, b, s),
            _ => return None,
        };
        if blur < 0.0 {
            return None; // a negative blur is invalid
        }
        if inset {
            diagnostics::unsupported::record("css-value", "box-shadow: inset");
        }
        out.push(BoxShadow {
            offset_x: offset_x.clamp(-extent, extent),
            offset_y: offset_y.clamp(-extent, extent),
            blur: blur.min(extent),
            spread: spread.clamp(-extent, extent),
            color: color.unwrap_or(current),
            inset,
        });
    }
    Some(out)
}

/// A `background-image` layer: `url(…)`, `none`, or (unsupported, so
/// counted and treated as `none`) a gradient or `image-set()`.
fn parse_background_image(value: &str) -> Option<String> {
    let first_layer = split_layers(value).into_iter().next().unwrap_or("");
    let v = first_layer.trim();
    if let Some(open) = v.find('(') {
        let name = v[..open].trim().to_ascii_lowercase();
        if name != "url" {
            diagnostics::unsupported::record("css-value", &format!("background-image: {name}()"));
            return None;
        }
    }
    parse_url(v)
}

/// Splits a comma-separated list of background layers (commas inside
/// parentheses do not split).
fn split_layers(value: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0usize, 0);
    for (i, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

fn parse_background_repeat(value: &str) -> Option<BackgroundRepeat> {
    let tokens: Vec<String> = value.split_whitespace().map(str::to_ascii_lowercase).collect();
    let axis = |t: &str| match t {
        "repeat" | "space" | "round" => Some(true),
        "no-repeat" => Some(false),
        _ => None,
    };
    let (x, y) = match tokens.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["repeat-x"] => (true, false),
        ["repeat-y"] => (false, true),
        [one] => {
            let r = axis(one)?;
            (r, r)
        }
        [a, b] => (axis(a)?, axis(b)?),
        _ => return None,
    };
    Some(match (x, y) {
        (true, true) => BackgroundRepeat::Repeat,
        (true, false) => BackgroundRepeat::RepeatX,
        (false, true) => BackgroundRepeat::RepeatY,
        (false, false) => BackgroundRepeat::NoRepeat,
    })
}

fn parse_background_size(value: &str, ctx: &LengthContext) -> Option<BackgroundSize> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "cover" => return Some(BackgroundSize::Cover),
        "contain" => return Some(BackgroundSize::Contain),
        _ => {}
    }
    let axis = |t: &str| -> Option<Option<Length>> {
        if t == "auto" {
            Some(None)
        } else {
            parse_length(t, ctx).filter(|l| l.px >= 0.0 && l.percent >= 0.0).map(Some)
        }
    };
    match split_values(&value).as_slice() {
        [w] => Some(match axis(w)? {
            None => BackgroundSize::Auto,
            w => BackgroundSize::Explicit(w, None),
        }),
        [w, h] => Some(match (axis(w)?, axis(h)?) {
            (None, None) => BackgroundSize::Auto,
            (w, h) => BackgroundSize::Explicit(w, h),
        }),
        _ => None,
    }
}

/// `background-position`: one or two values (keywords, lengths,
/// percentages — keywords may come in either order, `top left`), or the
/// edge-offset forms (`right 10px bottom 5px`). `None` if invalid.
fn parse_background_position(tokens: &[&str], ctx: &LengthContext) -> Option<BackgroundPosition> {
    let keyword = |t: &str| -> Option<(char, f32)> {
        match t.to_ascii_lowercase().as_str() {
            "left" => Some(('x', 0.0)),
            "right" => Some(('x', 100.0)),
            "top" => Some(('y', 0.0)),
            "bottom" => Some(('y', 100.0)),
            "center" => Some(('c', 50.0)),
            _ => None,
        }
    };
    let len = |t: &str| parse_length(t, ctx);
    match *tokens {
        [one] => match keyword(one) {
            Some(('y', p)) => Some(BackgroundPosition { x: Length::percent(50.0), y: Length::percent(p) }),
            Some((_, p)) => Some(BackgroundPosition { x: Length::percent(p), y: Length::percent(50.0) }),
            None => Some(BackgroundPosition { x: len(one)?, y: Length::percent(50.0) }),
        },
        [a, b] => match (keyword(a), keyword(b)) {
            // `top left` / `center right`: keywords in either order.
            (Some(('y', py)), Some((_, px))) | (Some(('c', px)), Some(('y', py))) => {
                Some(BackgroundPosition { x: Length::percent(px), y: Length::percent(py) })
            }
            // `center left`: the keyword naming an axis decides it.
            (Some(('c', _)), Some(('x', px))) => Some(BackgroundPosition { x: Length::percent(px), y: Length::percent(50.0) }),
            (Some((ka, pa)), Some((kb, pb))) if ka != 'y' && kb != 'x' => {
                Some(BackgroundPosition { x: Length::percent(pa), y: Length::percent(pb) })
            }
            (Some((ka, pa)), None) if ka != 'y' => Some(BackgroundPosition { x: Length::percent(pa), y: len(b)? }),
            (None, Some((kb, pb))) if kb != 'x' => Some(BackgroundPosition { x: len(a)?, y: Length::percent(pb) }),
            (None, None) => Some(BackgroundPosition { x: len(a)?, y: len(b)? }),
            _ => None,
        },
        // Edge offsets: `right 10px bottom 20px` (and 3-value forms,
        // where one keyword has no offset).
        [_, _, _] | [_, _, _, _] => {
            let mut x = None;
            let mut y = None;
            let mut i = 0;
            while i < tokens.len() {
                let (axis, p) = keyword(tokens[i])?;
                let offset = tokens.get(i + 1).and_then(|t| if keyword(t).is_none() { len(t) } else { None });
                i += if offset.is_some() { 2 } else { 1 };
                let offset = offset.unwrap_or(Length::ZERO);
                // From the far edge an offset counts backwards.
                let value = if p == 100.0 { Length { px: -offset.px, percent: 100.0 - offset.percent } } else { Length { px: offset.px, percent: p + offset.percent } };
                let axis = match axis {
                    'c' if x.is_none() => 'x',
                    'c' => 'y',
                    a => a,
                };
                let slot = if axis == 'x' { &mut x } else { &mut y };
                if slot.replace(value).is_some() {
                    return None;
                }
            }
            Some(BackgroundPosition { x: x.unwrap_or(Length::percent(50.0)), y: y.unwrap_or(Length::percent(50.0)) })
        }
        _ => None,
    }
}

/// The `background` shorthand (CSS Backgrounds 3 §3.10). Every sub-
/// property it does not mention is reset to its initial value. Only one
/// image layer is modelled: with several comma-separated layers the first
/// image is used and the colour comes from the last layer, where it must
/// be. Attachment and box keywords are accepted and ignored.
fn parse_background_shorthand(value: &str, style: &mut Style, ctx: &LengthContext) {
    let layers = split_layers(value);
    let mut color = crate::values::Color::TRANSPARENT;
    let mut image = None;
    let mut repeat = BackgroundRepeat::Repeat;
    let mut position = BackgroundPosition::default();
    let mut size = BackgroundSize::Auto;
    for (index, layer) in layers.iter().enumerate() {
        let is_first = index == 0;
        let is_last = index + 1 == layers.len();
        let tokens = split_values(layer);
        let mut position_tokens: Vec<&str> = Vec::new();
        let mut size_tokens: Vec<&str> = Vec::new();
        let mut repeat_tokens: Vec<&str> = Vec::new();
        let mut after_slash = false;
        for token in tokens {
            let lower = token.to_ascii_lowercase();
            if let Some(rest) = token.strip_prefix('/') {
                after_slash = true;
                if !rest.is_empty() {
                    size_tokens.push(rest);
                }
                continue;
            }
            if let Some((before, after)) = token.split_once('/').filter(|_| !token.contains('(')) {
                // `center/cover` with no spaces.
                if !before.is_empty() {
                    position_tokens.push(before);
                }
                after_slash = true;
                if !after.is_empty() {
                    size_tokens.push(after);
                }
                continue;
            }
            if after_slash && size_tokens.len() < 2 && (lower == "auto" || lower == "cover" || lower == "contain" || parse_length(&lower, ctx).is_some()) {
                size_tokens.push(token);
                continue;
            }
            after_slash = false;
            if matches!(lower.as_str(), "repeat" | "repeat-x" | "repeat-y" | "no-repeat" | "space" | "round") {
                repeat_tokens.push(token);
            } else if matches!(lower.as_str(), "scroll" | "fixed" | "local" | "border-box" | "padding-box" | "content-box" | "text") {
                // attachment / origin / clip: not modelled
            } else if lower == "none" {
                // no image in this layer
            } else if lower.starts_with("url(") || lower.contains("gradient(") || lower.starts_with("image-set(") {
                if is_first {
                    image = parse_background_image(token);
                }
            } else if matches!(lower.as_str(), "left" | "right" | "top" | "bottom" | "center") || parse_length(&lower, ctx).is_some() {
                position_tokens.push(token);
            } else if let Some(c) = parse_color(token, style.color) {
                if !is_last {
                    return; // a colour only in the final layer: invalid
                }
                color = c;
            } else {
                return; // unknown token: the whole declaration is invalid
            }
        }
        if is_first {
            if !repeat_tokens.is_empty() {
                match parse_background_repeat(&repeat_tokens.join(" ")) {
                    Some(r) => repeat = r,
                    None => return,
                }
            }
            if !position_tokens.is_empty() {
                match parse_background_position(&position_tokens, ctx) {
                    Some(p) => position = p,
                    None => return,
                }
            }
            if !size_tokens.is_empty() {
                match parse_background_size(&size_tokens.join(" "), ctx) {
                    Some(s) => size = s,
                    None => return,
                }
            }
        }
    }
    style.background_color = color;
    style.background_image_path = image;
    style.background_repeat = repeat;
    style.background_position = position;
    style.background_size = size;
}

fn parse_border_style(token: &str) -> Option<BorderStyle> {
    match token.to_ascii_lowercase().as_str() {
        "none" | "hidden" => Some(BorderStyle::None),
        "solid" => Some(BorderStyle::Solid),
        "dashed" => Some(BorderStyle::Dashed),
        "dotted" => Some(BorderStyle::Dotted),
        "double" => Some(BorderStyle::Double),
        // groove/ridge/inset/outset — still out of scope.
        _ => None,
    }
}

/// `border` / `border-top` …: `<width> <style> <color>` — real CSS allows
/// these in any order, so each whitespace-separated token is classified by
/// what it parses as, not by position. Unrecognized tokens are silently
/// skipped rather than rejecting the whole declaration. What the value
/// omits takes its initial value (width 0 here, no style, `currentcolor`).
fn parse_border_side(value: &str, current: Color, ctx: &LengthContext) -> BorderSpec {
    let mut side = BorderSpec::default();
    for token in split_values(value) {
        if let Some(w) = border_width_value(token, ctx) {
            side.width = w;
        } else if let Some(s) = parse_border_style(token) {
            side.style = s;
        } else if let Some(c) = parse_color(token, current) {
            side.color = Some(c);
        }
    }
    side
}

/// One to four values in box order (top, right, bottom, left), expanded to
/// four; `None` if any value fails to parse or there are more than four.
fn box_values<T: Copy>(value: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Edges<T>> {
    let values: Vec<T> = split_values(value).into_iter().map(&parse).collect::<Option<_>>()?;
    Some(match values[..] {
        [all] => Edges::all(all),
        [v, h] => Edges { top: v, right: h, bottom: v, left: h },
        [t, h, b] => Edges { top: t, right: h, bottom: b, left: h },
        [t, r, b, l] => Edges { top: t, right: r, bottom: b, left: l },
        _ => return None,
    })
}

fn set_sides<T: Copy>(border: &mut Edges<BorderSpec>, values: Edges<T>, set: impl Fn(&mut BorderSpec, T)) {
    set(&mut border.top, values.top);
    set(&mut border.right, values.right);
    set(&mut border.bottom, values.bottom);
    set(&mut border.left, values.left);
}

/// `top`, `right`, `bottom` or `left` from `border-<side>-…`.
fn side_name(property: &str) -> &str {
    property.trim_start_matches("border-").split('-').next().unwrap_or("")
}

fn border_side_mut<'a>(border: &'a mut Edges<BorderSpec>, side: &str) -> &'a mut BorderSpec {
    match side {
        "top" => &mut border.top,
        "right" => &mut border.right,
        "bottom" => &mut border.bottom,
        _ => &mut border.left,
    }
}

/// `outline: <width> <style> <color>` — same order-independent token
/// classification as `border`.
fn parse_outline_shorthand(value: &str, style: &mut Style, ctx: &LengthContext) {
    for token in split_values(value) {
        if let Some(w) = border_width_value(token, ctx) {
            style.outline_width = w;
        } else if let Some(s) = parse_border_style(token) {
            style.outline_style = s;
        } else if let Some(c) = parse_color(token, style.color) {
            style.outline_color = c;
        }
    }
}

/// Milestone 35: the text properties (`crate::text`), copy-on-write into
/// the element's shared text style. Invalid values are ignored.
fn apply_text_property(style: &mut Style, property: &str, value: &str, ctx: &LengthContext) {
    use crate::text::*;
    let font_size = style.font_size;
    let color = style.color;
    let parent_weight = style.text.font_weight;
    let v = value.trim().to_ascii_lowercase();
    let t = std::sync::Arc::make_mut(&mut style.text);
    match property {
        "font-family" => {
            if let Some(f) = parse_font_family(value) {
                t.font_family = f;
            }
        }
        "font-weight" => {
            if let Some(w) = parse_font_weight(value, parent_weight) {
                t.font_weight = w;
            }
        }
        "font-style" => {
            if let Some(s) = parse_font_style(value) {
                t.font_style = s;
            }
        }
        "line-height" => {
            if let Some(l) = parse_line_height(value, font_size, ctx) {
                t.line_height = l;
            }
        }
        "text-decoration" | "text-decoration-line" => {
            if let Some((lines, c)) = parse_text_decoration(value, color) {
                t.decoration.lines = t.inherited_lines | lines;
                if property == "text-decoration" {
                    t.decoration.color = c;
                }
            }
        }
        "text-decoration-color" => {
            if let Some(c) = crate::parse_color(value, color) {
                t.decoration.color = Some(c);
            }
        }
        "text-transform" => {
            t.text_transform = match v.as_str() {
                "none" => TextTransform::None,
                "uppercase" => TextTransform::Uppercase,
                "lowercase" => TextTransform::Lowercase,
                "capitalize" => TextTransform::Capitalize,
                _ => t.text_transform,
            }
        }
        "direction" => match v.as_str() {
            "ltr" => t.rtl = false,
            "rtl" => t.rtl = true,
            _ => {}
        },
        "unicode-bidi" => {
            t.unicode_bidi = match v.as_str() {
                "normal" => UnicodeBidi::Normal,
                "embed" => UnicodeBidi::Embed,
                "isolate" => UnicodeBidi::Isolate,
                "bidi-override" => UnicodeBidi::BidiOverride,
                "isolate-override" => UnicodeBidi::IsolateOverride,
                "plaintext" => UnicodeBidi::Plaintext,
                _ => t.unicode_bidi,
            }
        }
        "font" => {
            // The size was applied in the cascade's font-size pass.
            if let Some((style_kind, weight, _, line_height, family)) = parse_font_shorthand(value) {
                t.font_style = style_kind;
                t.font_weight = weight.and_then(|w| parse_font_weight(w, parent_weight)).unwrap_or(400);
                t.line_height = line_height.and_then(|l| parse_line_height(l, font_size, ctx)).unwrap_or(LineHeight::Normal);
                if let Some(f) = parse_font_family(family) {
                    t.font_family = f;
                }
            }
        }
        _ => {}
    }
}
