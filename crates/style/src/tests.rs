//! The full test suite for this crate. Kept as one file rather than
//! split per-module: the overwhelming majority of these tests exercise
//! the public, black-box entry points (`resolve_style`,
//! `resolve_style_in_context`, `apply_transitions`, `apply_animations`)
//! rather than any one module's internals in isolation — cascade
//! mechanics, declaration semantics, and selector matching are all
//! exercised together through the same handful of helpers
//! (`empty_sheet`, `style_of_inline`, `resolve_children`, ...), so a
//! forced per-module split would mostly just duplicate those helpers
//! without making any single test easier to find. `apply_declaration`
//! and `parse_easing` are the only genuinely private (`pub(crate)`)
//! items called directly rather than through a public entry point.

use super::*;
use crate::cascade::MAX_STYLE_DEPTH;
use crate::declarations::apply_declaration;
use crate::transitions::parse_easing;
use css_parser::parse_stylesheet;
use std::collections::HashMap;

fn empty_sheet() -> Stylesheet {
    Stylesheet::default()
}

#[test]
fn resolves_known_properties_from_inline_style_only() {
    let mut el = Element::new("div");
    el.attributes.push((
        "style".into(),
        "width:100px;height:50px;background-color:#ff0000;margin:5px;font-size:20px;color:#00ff00".into(),
    ));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.width, Some(Length::px(100.0)));
    assert_eq!(style.height, Some(Length::px(50.0)));
    assert_eq!(style.background_color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(style.margin.resolve(0.0), EdgeSizes::uniform(5.0));
    assert_eq!(style.font_size, 20.0);
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn missing_style_attribute_and_empty_stylesheet_yields_default() {
    let el = Element::new("div");
    let default_style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(default_style, Style::default());
    assert_eq!(default_style.font_size, 16.0);
    assert_eq!(default_style.color, Color { r: 0, g: 0, b: 0, a: 255 });
}

#[test]
fn named_color() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "background-color:blue".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.background_color, Color { r: 0, g: 0, b: 255, a: 255 });
}

#[test]
fn stylesheet_type_selector_applies() {
    let sheet = parse_stylesheet("div { color: #ff0000; }");
    let el = Element::new("div");
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn stylesheet_class_selector_applies() {
    let sheet = parse_stylesheet(".highlight { color: #00ff00; }");
    let mut el = Element::new("div");
    el.attributes.push(("class".into(), "highlight".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn stylesheet_id_selector_applies() {
    let sheet = parse_stylesheet("#hero { color: #0000ff; }");
    let mut el = Element::new("div");
    el.attributes.push(("id".into(), "hero".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 0, g: 0, b: 255, a: 255 });
}

#[test]
fn descendant_combinator_requires_matching_ancestor() {
    let sheet = parse_stylesheet("div span { color: #ff0000; }");
    let div = Element::new("div");
    let span = Element::new("span");
    let other = Element::new("p");

    let style = resolve_style(&span, &[&div], None, &sheet);
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 });

    let style2 = resolve_style(&span, &[&other], None, &sheet);
    assert_eq!(style2.color, Style::default().color);
}

#[test]
fn higher_specificity_wins_regardless_of_source_order() {
    let sheet = parse_stylesheet("#hero { color: #ff0000; } div { color: #00ff00; }");
    let mut el = Element::new("div");
    el.attributes.push(("id".into(), "hero".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn later_source_order_wins_at_equal_specificity() {
    let sheet = parse_stylesheet("div { color: #ff0000; } div { color: #00ff00; }");
    let el = Element::new("div");
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn inline_style_beats_any_stylesheet_specificity() {
    let sheet = parse_stylesheet("#hero { color: #ff0000; }");
    let mut el = Element::new("div");
    el.attributes.push(("id".into(), "hero".into()));
    el.attributes.push(("style".into(), "color:#00ff00".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn stylesheet_important_beats_inline_normal() {
    let sheet = parse_stylesheet("div { color: #ff0000 !important; }");
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "color:#00ff00".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn inline_important_beats_everything() {
    let sheet = parse_stylesheet("div { color: #ff0000 !important; }");
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "color:#00ff00 !important".into()));
    let style = resolve_style(&el, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn color_inherits_from_parent_computed_style() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "color:#ff0000".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());

    let child = Element::new("span");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn background_color_does_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "background-color:#ff0000".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());

    let child = Element::new("span");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.background_color, Color::TRANSPARENT);
}

#[test]
fn compound_type_and_class_selector_requires_both() {
    let sheet = parse_stylesheet("div.foo { color: #ff0000; }");
    let mut matching = Element::new("div");
    matching.attributes.push(("class".into(), "foo".into()));
    let style = resolve_style(&matching, &[], None, &sheet);
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 });

    let mut wrong_class = Element::new("div");
    wrong_class.attributes.push(("class".into(), "bar".into()));
    let style2 = resolve_style(&wrong_class, &[], None, &sheet);
    assert_eq!(style2.color, Style::default().color);

    let mut wrong_tag = Element::new("span");
    wrong_tag.attributes.push(("class".into(), "foo".into()));
    let style3 = resolve_style(&wrong_tag, &[], None, &sheet);
    assert_eq!(style3.color, Style::default().color);
}

#[test]
fn text_align_parses_and_defaults_to_left() {
    let el = Element::new("div");
    assert_eq!(resolve_style(&el, &[], None, &empty_sheet()).text_align, TextAlign::Start);

    let mut center_el = Element::new("div");
    center_el.attributes.push(("style".into(), "text-align:center".into()));
    assert_eq!(resolve_style(&center_el, &[], None, &empty_sheet()).text_align, TextAlign::Center);

    let mut right_el = Element::new("div");
    right_el.attributes.push(("style".into(), "text-align:right".into()));
    assert_eq!(resolve_style(&right_el, &[], None, &empty_sheet()).text_align, TextAlign::Right);
}

#[test]
fn text_align_inherits_from_parent() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "text-align:center".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());

    let child = Element::new("p");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.text_align, TextAlign::Center);
}

#[test]
fn padding_shorthand_one_value_applies_to_all_sides() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "padding:10px".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.padding.resolve(0.0), EdgeSizes::uniform(10.0));
}

#[test]
fn padding_shorthand_two_values_vertical_horizontal() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "padding:10px 20px".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.padding.resolve(0.0), EdgeSizes { top: 10.0, bottom: 10.0, left: 20.0, right: 20.0 });
}

#[test]
fn padding_shorthand_four_values_top_right_bottom_left() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "padding:1px 2px 3px 4px".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.padding.resolve(0.0), EdgeSizes { top: 1.0, right: 2.0, bottom: 3.0, left: 4.0 });
}

#[test]
fn margin_shorthand_three_values() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "margin:1px 2px 3px".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.margin.resolve(0.0), EdgeSizes { top: 1.0, left: 2.0, right: 2.0, bottom: 3.0 });
}

#[test]
fn longhand_and_shorthand_padding_agree() {
    let mut shorthand = Element::new("div");
    shorthand.attributes.push(("style".into(), "padding:5px 10px".into()));
    let s1 = resolve_style(&shorthand, &[], None, &empty_sheet());

    let mut longhand = Element::new("div");
    longhand.attributes.push((
        "style".into(),
        "padding-top:5px;padding-bottom:5px;padding-left:10px;padding-right:10px".into(),
    ));
    let s2 = resolve_style(&longhand, &[], None, &empty_sheet());

    assert_eq!(s1.padding, s2.padding);
}

#[test]
fn longhand_and_shorthand_margin_agree() {
    let mut shorthand = Element::new("div");
    shorthand.attributes.push(("style".into(), "margin:8px".into()));
    let s1 = resolve_style(&shorthand, &[], None, &empty_sheet());

    let mut longhand = Element::new("div");
    longhand.attributes.push((
        "style".into(),
        "margin-top:8px;margin-right:8px;margin-bottom:8px;margin-left:8px".into(),
    ));
    let s2 = resolve_style(&longhand, &[], None, &empty_sheet());

    assert_eq!(s1.margin, s2.margin);
}

#[test]
fn border_longhands_parse() {
    let mut el = Element::new("div");
    el.attributes.push((
        "style".into(),
        "border-width:2px;border-color:#ff0000;border-style:solid".into(),
    ));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.border().top.width, 2.0);
    assert_eq!(style.border().top.color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(style.border().top.style, BorderStyle::Solid);
}

#[test]
fn border_shorthand_parses_any_token_order() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "border: solid 3px #00ff00".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.border().top.width, 3.0);
    assert_eq!(style.border().top.color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(style.border().top.style, BorderStyle::Solid);
}

#[test]
fn border_width_without_border_style_means_no_visible_border() {
    // Matches real CSS: border-width alone (default border-style
    // is `none`) has no visible effect. Callers (layout) are
    // expected to check border_style, not just border_width > 0.
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "border-width:5px;border-color:#ff0000".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.border().top.style, BorderStyle::None);
}

#[test]
fn box_sizing_parses() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "box-sizing:border-box".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.box_sizing, BoxSizing::BorderBox);

    let default_style = resolve_style(&Element::new("div"), &[], None, &empty_sheet());
    assert_eq!(default_style.box_sizing, BoxSizing::ContentBox);
}

#[test]
fn box_model_properties_do_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "padding:20px;margin:20px;border:5px solid #ff0000".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());

    let child = Element::new("div");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.padding.resolve(0.0), EdgeSizes::default());
    assert_eq!(child_style.margin.resolve(0.0), EdgeSizes::default());
    assert_eq!(child_style.border().top.style, BorderStyle::None);
}

#[test]
fn background_image_url_parses_with_and_without_quotes() {
    let mut a = Element::new("div");
    a.attributes.push(("style".into(), "background-image:url(photo.png)".into()));
    assert_eq!(resolve_style(&a, &[], None, &empty_sheet()).background_image_path, Some("photo.png".to_string()));

    let mut b = Element::new("div");
    b.attributes.push(("style".into(), r#"background-image:url("photo.png")"#.into()));
    assert_eq!(resolve_style(&b, &[], None, &empty_sheet()).background_image_path, Some("photo.png".to_string()));

    let mut c = Element::new("div");
    c.attributes.push(("style".into(), "background-image:url('photo.png')".into()));
    assert_eq!(resolve_style(&c, &[], None, &empty_sheet()).background_image_path, Some("photo.png".to_string()));
}

#[test]
fn background_image_none_clears_it() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "background-image:none".into()));
    assert_eq!(resolve_style(&el, &[], None, &empty_sheet()).background_image_path, None);
}

#[test]
fn background_image_does_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "background-image:url(photo.png)".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());

    let child = Element::new("div");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.background_image_path, None);
}

#[test]
fn flex_container_properties_parse() {
    let mut el = Element::new("div");
    el.attributes.push((
        "style".into(),
        "display:flex;flex-direction:column;justify-content:space-between;align-items:center;gap:10px".into(),
    ));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.display, Display::Flex);
    assert_eq!(style.flex_direction, FlexDirection::Column);
    assert_eq!(style.justify_content, JustifyContent::SpaceBetween);
    assert_eq!(style.align_items, AlignItems::Center);
    assert_eq!(style.gap, 10.0);
}

#[test]
fn flex_item_longhands_parse() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "flex-grow:2;flex-shrink:0;flex-basis:50px".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.flex_grow, 2.0);
    assert_eq!(style.flex_shrink, 0.0);
    assert_eq!(style.flex_basis, Some(Length::px(50.0)));
}

#[test]
fn flex_shorthand_single_number_sets_grow_shrink_basis() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "flex:2".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.flex_grow, 2.0);
    assert_eq!(style.flex_shrink, 1.0);
    assert_eq!(style.flex_basis, Some(Length::px(0.0)));
}

#[test]
fn flex_shorthand_none_disables_grow_and_shrink() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "flex:none".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.flex_grow, 0.0);
    assert_eq!(style.flex_shrink, 0.0);
    assert_eq!(style.flex_basis, None);
}

#[test]
fn flex_properties_default_correctly_and_do_not_inherit() {
    let default_style = resolve_style(&Element::new("div"), &[], None, &empty_sheet());
    assert_eq!(default_style.display, Display::Block);
    assert_eq!(default_style.flex_grow, 0.0);
    assert_eq!(default_style.flex_shrink, 1.0); // CSS initial value

    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "display:flex".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());
    let child = Element::new("div");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.display, Display::Block); // did not inherit "flex"
}

#[test]
fn transform_defaults_to_identity() {
    let default_style = resolve_style(&Element::new("div"), &[], None, &empty_sheet());
    assert_eq!(default_style.transform, Transform::default());
    assert_eq!(default_style.transform.scale_x, 1.0);
    assert_eq!(default_style.transform.translate_x, 0.0);
}

#[test]
fn transform_translate_parses_both_axes() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:translate(10px, 20px)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.translate_x, 10.0);
    assert_eq!(style.transform.translate_y, 20.0);
}

#[test]
fn transform_translate_x_and_y_individually() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:translateX(5px)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.translate_x, 5.0);
    assert_eq!(style.transform.translate_y, 0.0);
}

#[test]
fn transform_scale_single_value_applies_to_both_axes() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:scale(2)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.scale_x, 2.0);
    assert_eq!(style.transform.scale_y, 2.0);
}

#[test]
fn transform_scale_two_values_are_independent() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:scale(2, 0.5)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.scale_x, 2.0);
    assert_eq!(style.transform.scale_y, 0.5);
}

#[test]
fn transform_chain_composes_translate_and_scale() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:translate(10px, 0px) scale(2)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.translate_x, 10.0);
    assert_eq!(style.transform.scale_x, 2.0);
}

#[test]
fn transform_unsupported_function_is_skipped_not_a_panic() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:matrix(1,0,0,1,0,0) translate(5px,5px)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    // matrix() is genuinely unsupported and ignored; the translate()
    // after it still applies (Milestone 22: rotate()/skew() moved
    // OUT of the "unsupported" bucket — see the tests below).
    assert_eq!(style.transform.translate_x, 5.0);
}

#[test]
fn transform_rotate_parses_into_rotate_deg() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:rotate(45deg)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.rotate_deg, 45.0);
}

#[test]
fn transform_skew_parses_both_axes() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:skew(10deg, 20deg)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.skew_x_deg, 10.0);
    assert_eq!(style.transform.skew_y_deg, 20.0);
}

#[test]
fn transform_skew_x_and_y_individually() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:skewX(5deg) skewY(15deg)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.skew_x_deg, 5.0);
    assert_eq!(style.transform.skew_y_deg, 15.0);
}

#[test]
fn transform_rotate_composes_with_translate_and_scale() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transform:translate(5px,5px) rotate(45deg) scale(2)".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transform.translate_x, 5.0);
    assert_eq!(style.transform.rotate_deg, 45.0);
    assert_eq!(style.transform.scale_x, 2.0);
}

// ---- Milestone 22: transition/easing parsing ----

#[test]
fn transition_shorthand_parses_property_duration_and_easing() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transition: background-color 0.3s ease-in-out".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transitions.len(), 1);
    assert_eq!(style.transitions[0].property, TransitionProperty::BackgroundColor);
    assert!((style.transitions[0].duration - 0.3).abs() < 0.001);
    assert_eq!(style.transitions[0].easing, Easing::EASE_IN_OUT);
}

#[test]
fn transition_duration_accepts_milliseconds() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transition: transform 500ms".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert!((style.transitions[0].duration - 0.5).abs() < 0.001);
}

#[test]
fn transition_defaults_to_ease_when_no_easing_given() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transition: width 1s".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transitions[0].easing, Easing::EASE);
}

#[test]
fn transition_shorthand_parses_multiple_comma_separated_entries() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transition: background-color 0.3s, transform 0.5s linear".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transitions.len(), 2);
    assert_eq!(style.transitions[0].property, TransitionProperty::BackgroundColor);
    assert_eq!(style.transitions[1].property, TransitionProperty::Transform);
    assert_eq!(style.transitions[1].easing, Easing::Linear);
}

#[test]
fn transition_all_keyword_parses() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "transition: all 0.2s".into()));
    let style = resolve_style(&el, &[], None, &empty_sheet());
    assert_eq!(style.transitions[0].property, TransitionProperty::All);
}

#[test]
fn transition_does_not_inherit() {
    let mut parent_style = Style::default();
    parent_style.transitions = vec![TransitionSpec { property: TransitionProperty::All, duration: 1.0, easing: Easing::Linear }];
    let child = Element::new("span");
    let child_style = resolve_style(&child, &[], Some(&parent_style), &empty_sheet());
    assert!(child_style.transitions.is_empty());
}

#[test]
fn cubic_bezier_easing_parses_arbitrary_control_points() {
    assert_eq!(parse_easing("cubic-bezier(0.1, 0.2, 0.3, 0.4)"), Some(Easing::CubicBezier(0.1, 0.2, 0.3, 0.4)));
    assert_eq!(parse_easing("not-a-real-easing"), None);
}

#[test]
fn linear_easing_is_the_identity_function() {
    assert_eq!(Easing::Linear.apply(0.0), 0.0);
    assert_eq!(Easing::Linear.apply(0.5), 0.5);
    assert_eq!(Easing::Linear.apply(1.0), 1.0);
}

#[test]
fn cubic_bezier_easing_endpoints_are_fixed_at_zero_and_one() {
    // Every real CSS easing curve passes through (0,0) and (1,1)
    // regardless of control points — a basic sanity check on the
    // bisection solver, not just trusting the math.
    for easing in [Easing::EASE, Easing::EASE_IN, Easing::EASE_OUT, Easing::EASE_IN_OUT] {
        assert!(easing.apply(0.0).abs() < 0.001, "{easing:?} should start at 0");
        assert!((easing.apply(1.0) - 1.0).abs() < 0.001, "{easing:?} should end at 1");
    }
}

#[test]
fn ease_in_starts_slower_than_linear() {
    // ease-in's defining characteristic: slow start, fast end —
    // at t=0.5 it should still be well BEHIND linear's 0.5.
    assert!(Easing::EASE_IN.apply(0.5) < 0.4);
}

#[test]
fn ease_out_starts_faster_than_linear() {
    // ease-out's defining characteristic: fast start, slow end —
    // at t=0.5 it should already be well AHEAD of linear's 0.5.
    assert!(Easing::EASE_OUT.apply(0.5) > 0.6);
}

// ---- Milestone 22: apply_transitions (the interpolation engine) ----

fn cache_with(el: &Element, style: Style) -> StyleCache {
    let mut cache = StyleCache::default();
    cache.insert(el.node_id, style);
    cache
}

fn style_in<'a>(cache: &'a StyleCache, el: &Element) -> &'a Style {
    cache.get(&el.node_id).expect("element should be in the cache")
}

#[test]
fn apply_transitions_passes_through_unchanged_on_the_first_frame() {
    // Real CSS doesn't transition on initial paint either — there's
    // no "previous value" to animate FROM yet.
    let el = Element::new("div");
    let mut style = Style::default();
    style.background_color = Color { r: 10, g: 20, b: 30, a: 255 };
    style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];
    let cache = cache_with(&el, style.clone());

    let mut registry = TransitionRegistry::new();
    let displayed = apply_transitions(&el, cache.clone(), &mut registry, std::time::Instant::now());
    assert_eq!(style_in(&displayed, &el).background_color, style.background_color);
}

#[test]
fn apply_transitions_interpolates_partway_through_the_duration() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];

    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();

    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };
    let frame1 = cache_with(&el, style.clone());
    apply_transitions(&el, frame1.clone(), &mut registry, start); // establishes the baseline

    style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    let frame2 = cache_with(&el, style.clone());
    // The FIRST frame after a target change always shows the FROM
    // value (t=0 of the new transition) — this call starts it.
    apply_transitions(&el, frame2.clone(), &mut registry, start);
    // A LATER frame with the SAME target is what actually
    // progresses the interpolation.
    let mid = start + std::time::Duration::from_millis(500);
    let displayed = apply_transitions(&el, frame2.clone(), &mut registry, mid);

    let r = style_in(&displayed, &el).background_color.r;
    assert!(r > 0 && r < 100, "should be partway through a linear 1s transition at t=0.5s, got r={r}");
    assert!((45..=55).contains(&r), "linear easing at t=0.5 should be close to the midpoint, got r={r}");
}

#[test]
fn apply_transitions_snaps_to_target_once_duration_elapses() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];

    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();

    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);

    style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    let frame2 = cache_with(&el, style.clone());
    apply_transitions(&el, frame2.clone(), &mut registry, start); // starts the transition
    let after = start + std::time::Duration::from_millis(1500); // past the 1s duration
    let displayed = apply_transitions(&el, frame2.clone(), &mut registry, after);

    assert_eq!(style_in(&displayed, &el).background_color, style.background_color);
}

#[test]
fn apply_transitions_interrupting_restarts_from_the_current_displayed_value() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];

    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();

    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);

    style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start); // starts 0 -> 100

    // Halfway to 100, the target changes AGAIN, to 200 (clamped
    // concerns aside — this is just a plain f32-ish u8 math check).
    let mid = start + std::time::Duration::from_millis(500);
    let mid_displayed = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, mid);
    let r_at_interrupt = style_in(&mid_displayed, &el).background_color.r;
    assert!(r_at_interrupt > 0 && r_at_interrupt < 100, "sanity: really was mid-flight when interrupted");

    style.background_color = Color { r: 200, g: 0, b: 0, a: 255 };
    let interrupted = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, mid);
    // The instant a new transition starts, it displays its FROM
    // value — which must be r_at_interrupt (the current visual
    // state), not 0 (the original) and not 100 (the old target).
    assert_eq!(style_in(&interrupted, &el).background_color.r, r_at_interrupt);
}

#[test]
fn a_property_with_no_transition_spec_snaps_immediately() {
    let el = Element::new("div");
    let mut style = Style::default(); // no `transitions` at all
    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };
    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);

    style.background_color = Color { r: 200, g: 0, b: 0, a: 255 };
    let displayed = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);
    assert_eq!(style_in(&displayed, &el).background_color.r, 200, "no transition declared — must snap, not animate");
}

#[test]
fn width_auto_to_a_length_snaps_instead_of_interpolating() {
    // Real CSS doesn't interpolate to/from `auto` either — there's
    // no defined numeric value to animate FROM.
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::Width, duration: 1.0, easing: Easing::Linear }];
    style.width = None;
    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);

    style.width = Some(Length::px(100.0));
    let mid = start + std::time::Duration::from_millis(500);
    let displayed = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, mid);
    assert_eq!(style_in(&displayed, &el).width, Some(Length::px(100.0)), "auto -> length must snap, not interpolate");
}

#[test]
fn transition_all_covers_a_property_not_explicitly_named() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::All, duration: 1.0, easing: Easing::Linear }];
    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };
    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);

    style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start); // starts 0 -> 100

    let mid = start + std::time::Duration::from_millis(500);
    let displayed = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, mid);
    let r = style_in(&displayed, &el).background_color.r;
    assert!(r > 0 && r < 100, "transition: all should cover background-color too, got r={r}");
}

#[test]
fn nested_elements_transition_independently() {
    let child = Element::new("span");
    let mut child_style = Style::default();
    child_style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];
    child_style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };

    let mut root = Element::new("div");
    let mut root_style = Style::default();
    root_style.background_color = Color { r: 50, g: 50, b: 50, a: 255 }; // no transition on the root

    // Push the clone that will actually live in the tree FIRST, then key
    // the frame caches off the node_id of the element really in the tree.
    root.children.push(dom::Node::Element(child.clone()));
    let child_id = match &root.children[0] {
        dom::Node::Element(el) => el.node_id,
        _ => unreachable!(),
    };

    let mut frame1 = StyleCache::default();
    frame1.insert(root.node_id, root_style.clone());
    frame1.insert(child_id, child_style.clone());

    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&root, frame1.clone(), &mut registry, start);

    child_style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    let mut frame2 = StyleCache::default();
    frame2.insert(root.node_id, root_style.clone());
    frame2.insert(child_id, child_style.clone());
    apply_transitions(&root, frame2.clone(), &mut registry, start); // starts child's 0 -> 100

    let mid = start + std::time::Duration::from_millis(500);
    let displayed = apply_transitions(&root, frame2.clone(), &mut registry, mid);

    // Root (no transition) snaps; child (has one) is mid-flight.
    assert_eq!(displayed.get(&root.node_id).unwrap().background_color, root_style.background_color);
    let child_r = displayed.get(&child_id).unwrap().background_color.r;
    assert!(child_r > 0 && child_r < 100);
}

// ---- Milestone 22: animation-* parsing ----

#[test]
fn animation_name_duration_and_iteration_count_parse() {
    let mut style = Style::default();
    apply_declaration(&mut style, "animation-name", "spin", crate::ViewportSize::DEFAULT);
    apply_declaration(&mut style, "animation-duration", "2s", crate::ViewportSize::DEFAULT);
    apply_declaration(&mut style, "animation-iteration-count", "3", crate::ViewportSize::DEFAULT);
    assert_eq!(style.animation_name, Some("spin".to_string()));
    assert_eq!(style.animation_duration, 2.0);
    assert_eq!(style.animation_iteration_count, Some(3.0));
}

#[test]
fn animation_iteration_count_infinite_parses_to_none() {
    let mut style = Style::default();
    apply_declaration(&mut style, "animation-iteration-count", "infinite", crate::ViewportSize::DEFAULT);
    assert_eq!(style.animation_iteration_count, None);
}

#[test]
fn animation_name_none_clears_it() {
    let mut style = Style::default();
    apply_declaration(&mut style, "animation-name", "spin", crate::ViewportSize::DEFAULT);
    apply_declaration(&mut style, "animation-name", "none", crate::ViewportSize::DEFAULT);
    assert_eq!(style.animation_name, None);
}

#[test]
fn animation_shorthand_parses_name_duration_and_infinite() {
    let mut style = Style::default();
    apply_declaration(&mut style, "animation", "spin 2s linear infinite", crate::ViewportSize::DEFAULT);
    assert_eq!(style.animation_name, Some("spin".to_string()));
    assert_eq!(style.animation_duration, 2.0);
    assert_eq!(style.animation_iteration_count, None);
}

#[test]
fn animation_does_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "animation: spin 1s".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());
    let child = Element::new("div");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.animation_name, None);
}

// ---- Milestone 22: apply_animations (the @keyframes engine) ----

fn keyframes_map(entries: &[KeyframesRule]) -> HashMap<String, KeyframesRule> {
    entries.iter().cloned().map(|k| (k.name.clone(), k)).collect()
}

/// The `background-color` red channel and `width` of an element animated
/// by `css`'s `@keyframes a`, `secs` into a 1 s animation, with `timing` as
/// its `animation-timing-function`.
fn animated_at(css: &str, timing: &str, secs: f32) -> (u8, Option<f32>) {
    let sheet = css_parser::parse_stylesheet(css);
    let el = Element::new("div");
    let mut style = resolve_style(&el, &[], None, &Stylesheet::default());
    style.animation_name = Some("a".into());
    style.animation_duration = 1.0;
    style.animation_timing_function = crate::transitions::parse_easing(timing).unwrap();
    style.width = Some(Length::px(100.0));
    let keyframes = keyframes_map(&sheet.keyframes);
    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);
    let then = start + std::time::Duration::from_secs_f32(secs);
    let out = apply_animations(&el, cache_with(&el, style), &keyframes, &mut registry, then);
    let s = style_in(&out, &el);
    (s.background_color.r, s.width.map(|w| w.px))
}

/// M22's gap: a middle stop is a real stop — the value goes through it.
#[test]
fn a_middle_keyframe_is_passed_through() {
    let css = "@keyframes a { 0% { background-color: #000000 } 50% { background-color: #ff0000 } 100% { background-color: #000000 } }";
    assert_eq!(animated_at(css, "linear", 0.5).0, 255, "at the middle stop");
    let quarter = animated_at(css, "linear", 0.25).0;
    assert!((120..=135).contains(&quarter), "half-way to it: {quarter}");
    let three_quarters = animated_at(css, "linear", 0.75).0;
    assert!((120..=135).contains(&three_quarters), "half-way back: {three_quarters}");
}

/// Each property runs on the stops that declare it: a stop setting only
/// `width` does not pull `background-color` back to the element's own.
#[test]
fn each_property_runs_on_its_own_keyframes() {
    let css = "@keyframes a { 0% { background-color: #000000 } 50% { width: 10px } 100% { background-color: #c80000 } }";
    let (red, width) = animated_at(css, "linear", 0.5);
    assert!((95..=105).contains(&red), "background half-way from 0 to 200: {red}");
    assert_eq!(width, Some(10.0), "width at its own stop");
    let (_, width) = animated_at(css, "linear", 0.75);
    assert_eq!(width, Some(55.0), "half-way from 10 back to the element's own 100");
}

/// `animation-timing-function` paces each segment; a stop's own one wins
/// for the segment it starts.
#[test]
fn the_timing_function_paces_each_segment() {
    let css = "@keyframes a { from { background-color: #000000 } to { background-color: #c80000 } }";
    let linear = animated_at(css, "linear", 0.5).0;
    let eased_in = animated_at(css, "ease-in", 0.5).0;
    assert!((95..=105).contains(&linear), "{linear}");
    assert!(eased_in < 80, "ease-in is behind linear at the middle: {eased_in}");
    let own = "@keyframes a { from { background-color: #000000; animation-timing-function: linear } to { background-color: #c80000 } }";
    assert!((95..=105).contains(&animated_at(own, "ease-in", 0.5).0), "the stop's own timing function wins");
}

/// An infinite animation of nothing this engine animates (`opacity`) is
/// not in flight: it must not keep the page animating (found on
/// mozilla.org's `pulse`, once its `93.75%, to` stop list parsed).
#[test]
fn an_animation_of_nothing_animatable_is_not_active() {
    let sheet = css_parser::parse_stylesheet("@keyframes pulse { 93.75%, to { opacity: .2 } }");
    let el = Element::new("div");
    let style = Style { animation_name: Some("pulse".into()), animation_duration: 0.8, animation_iteration_count: None, ..Style::default() };
    let mut registry = AnimationRegistry::new();
    apply_animations(&el, cache_with(&el, style), &keyframes_map(&sheet.keyframes), &mut registry, std::time::Instant::now());
    assert!(!registry.any_active());
}

#[test]
fn animation_timing_function_parses_alone_and_in_the_shorthand() {
    use crate::transitions::Easing;
    assert_eq!(style_of_inline("animation-timing-function: linear").animation_timing_function, Easing::Linear);
    assert_eq!(style_of_inline("animation: spin 2s ease-in infinite").animation_timing_function, Easing::EASE_IN);
    let s = style_of_inline("animation: spin 2s 1s cubic-bezier(0.1,0.2,0.3,0.4)");
    assert_eq!((s.animation_duration, s.animation_timing_function), (2.0, Easing::CubicBezier(0.1, 0.2, 0.3, 0.4)), "the second time is a delay");
    assert_eq!(Style::default().animation_timing_function, Easing::EASE, "CSS's default");
}

#[test]
fn apply_animations_interpolates_partway_through_a_cycle() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.animation_name = Some("fade".to_string());
    style.animation_duration = 1.0;
    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };

    let kf = KeyframesRule {
        name: "fade".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "background-color".into(), value: "#000000".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "background-color".into(), value: "#640000".into(), important: false }] }],
    };
    let keyframes = keyframes_map(&[kf]);

    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);

    let mid = start + std::time::Duration::from_millis(500);
    let displayed = apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, mid);
    let r = style_in(&displayed, &el).background_color.r;
    assert!(r > 0 && r < 100, "expected partway through the keyframe cycle, got r={r}");
}

#[test]
fn apply_animations_loops_when_iteration_count_is_infinite() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.animation_name = Some("fade".to_string());
    style.animation_duration = 1.0;
    style.animation_iteration_count = None; // infinite

    let kf = KeyframesRule {
        name: "fade".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "background-color".into(), value: "#000000".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "background-color".into(), value: "#640000".into(), important: false }] }],
    };
    let keyframes = keyframes_map(&[kf]);

    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);

    // 1.5 cycles in — should be halfway through the SECOND loop,
    // not frozen at the end of the first.
    let later = start + std::time::Duration::from_millis(1500);
    let displayed = apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, later);
    let r = style_in(&displayed, &el).background_color.r;
    assert!(r > 0 && r < 100, "infinite animation should still be mid-flight on its second loop, got r={r}");
}

#[test]
fn apply_animations_holds_at_the_to_stop_once_iterations_are_exhausted() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.animation_name = Some("fade".to_string());
    style.animation_duration = 1.0;
    style.animation_iteration_count = Some(1.0);

    let kf = KeyframesRule {
        name: "fade".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "background-color".into(), value: "#000000".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "background-color".into(), value: "#640000".into(), important: false }] }],
    };
    let keyframes = keyframes_map(&[kf]);

    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);

    let after = start + std::time::Duration::from_millis(5000);
    let displayed = apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, after);
    assert_eq!(style_in(&displayed, &el).background_color.r, 100);
}

#[test]
fn apply_animations_restarts_when_animation_name_changes() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.animation_name = Some("fade".to_string());
    style.animation_duration = 1.0;
    style.background_color = Color { r: 0, g: 0, b: 0, a: 255 };

    let fade = KeyframesRule {
        name: "fade".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "background-color".into(), value: "#000000".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "background-color".into(), value: "#640000".into(), important: false }] }],
    };
    let grow = KeyframesRule {
        name: "grow".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "width".into(), value: "10px".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "width".into(), value: "200px".into(), important: false }] }],
    };
    let keyframes = keyframes_map(&[fade, grow]);

    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);

    // Switch to a different animation 900ms in — it must restart
    // from ITS OWN t=0, not continue "fade"'s near-complete clock.
    style.animation_name = Some("grow".to_string());
    style.width = Some(Length::px(10.0));
    let switch = start + std::time::Duration::from_millis(900);
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, switch);

    let just_after_switch = switch + std::time::Duration::from_millis(10);
    let displayed = apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, just_after_switch);
    let w = style_in(&displayed, &el).width.unwrap().px;
    assert!(w < 20.0, "just-restarted 'grow' animation should be near its start, got width={w}");
}

#[test]
fn apply_animations_ignores_an_element_with_no_animation_name() {
    let el = Element::new("div");
    let style = Style::default();
    let keyframes: HashMap<String, KeyframesRule> = HashMap::new();
    let mut registry = AnimationRegistry::new();
    let displayed = apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, std::time::Instant::now());
    assert_eq!(style_in(&displayed, &el).background_color, style.background_color);
}

#[test]
fn animation_registry_any_active_reflects_in_progress_vs_exhausted() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.animation_name = Some("fade".to_string());
    style.animation_duration = 1.0;
    style.animation_iteration_count = Some(1.0);
    let kf = KeyframesRule {
        name: "fade".to_string(),
        stops: vec![css_parser::Keyframe { offset: 0.0, declarations: vec![Declaration { property: "background-color".into(), value: "#000000".into(), important: false }] }, css_parser::Keyframe { offset: 1.0, declarations: vec![Declaration { property: "background-color".into(), value: "#640000".into(), important: false }] }],
    };
    let keyframes = keyframes_map(&[kf]);
    let mut registry = AnimationRegistry::new();
    let start = std::time::Instant::now();
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, start);
    assert!(registry.any_active(), "just-started finite animation should be active");

    let after = start + std::time::Duration::from_millis(5000);
    apply_animations(&el, cache_with(&el, style.clone()), &keyframes, &mut registry, after);
    assert!(!registry.any_active(), "exhausted finite animation should no longer be active");
}

#[test]
fn transition_registry_any_active_reflects_in_progress_vs_settled() {
    let el = Element::new("div");
    let mut style = Style::default();
    style.transitions = vec![TransitionSpec { property: TransitionProperty::BackgroundColor, duration: 1.0, easing: Easing::Linear }];
    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);
    assert!(!registry.any_active(), "no target change yet — nothing should be in flight");

    style.background_color = Color { r: 100, g: 0, b: 0, a: 255 };
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);
    assert!(registry.any_active(), "just-triggered transition should be in flight");

    let after = start + std::time::Duration::from_millis(5000);
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, after);
    assert!(!registry.any_active(), "completed transition should no longer be in flight");
}

#[test]
fn transform_does_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "transform:scale(2)".into()));
    let parent_style = resolve_style(&parent, &[], None, &empty_sheet());
    let child = Element::new("div");
    let child_style = resolve_style(&child, &[&parent], Some(&parent_style), &empty_sheet());
    assert_eq!(child_style.transform, Transform::default());
}

// ---- Parallel style resolution ----

fn build_synthetic_tree(depth: usize, breadth: usize) -> Element {
    let mut root = Element::new("div");
    if depth == 0 {
        return root;
    }
    for i in 0..breadth {
        let mut child = build_synthetic_tree(depth - 1, breadth);
        // Give each node distinguishing content so cascade
        // matching/inheritance actually has something to get
        // wrong if the parallel path were buggy.
        child.attributes.push(("class".into(), format!("item-{i}")));
        child.attributes.push(("style".into(), format!("font-size:{}px", 10 + i)));
        root.children.push(dom::Node::Element(child));
    }
    root
}

/// Walks a tree sequentially, calling the ORIGINAL single-element
/// `resolve_style` at every node — the trusted reference
/// implementation the parallel path is checked against.
fn resolve_all_sequential(
    element: &Element,
    ancestors: &[&Element],
    parent_style: Option<&Style>,
    stylesheet: &Stylesheet,
    out: &mut StyleCache,
) {
    let style = resolve_style(element, ancestors, parent_style, stylesheet);
    out.insert(element.node_id, style.clone());

    let mut child_ancestors = vec![element];
    child_ancestors.extend_from_slice(ancestors);

    for child in &element.children {
        if let dom::Node::Element(child_el) = child {
            resolve_all_sequential(child_el, &child_ancestors, Some(&style), stylesheet, out);
        }
    }
}

#[test]
fn parallel_resolution_matches_sequential_on_a_small_tree() {
    let sheet = parse_stylesheet(".item-1 { color: #ff0000; } div span { text-align: center; }");
    let tree = build_synthetic_tree(3, 4);

    let parallel = resolve_style_tree_parallel(&tree, &sheet, DEFAULT_VIEWPORT_WIDTH);
    let mut sequential = StyleCache::default();
    resolve_all_sequential(&tree, &[], None, &sheet, &mut sequential);

    assert_eq!(parallel.len(), sequential.len());
    for (key, seq_style) in &sequential {
        assert_eq!(parallel.get(key), Some(seq_style), "mismatch at key {key}");
    }
}

#[test]
fn parallel_resolution_matches_sequential_on_a_larger_tree() {
    // Deep + bushy enough that rayon's work-stealing pool actually
    // has real parallel work to distribute across cores, not just
    // a handful of nodes — this is as close to a "did the
    // parallel path actually get exercised meaningfully" check as
    // a deterministic unit test can assert (wall-clock speedup
    // itself isn't something a unit test should assert on, since
    // that's inherently environment-dependent).
    let sheet = parse_stylesheet("div { color: #00ff00; } .item-2 { font-size: 30px; }");
    let tree = build_synthetic_tree(5, 5); // up to 5^5 = 3125 nodes

    let parallel = resolve_style_tree_parallel(&tree, &sheet, DEFAULT_VIEWPORT_WIDTH);
    let mut sequential = StyleCache::default();
    resolve_all_sequential(&tree, &[], None, &sheet, &mut sequential);

    assert_eq!(parallel.len(), sequential.len());
    assert!(parallel.len() > 100, "synthetic tree should have produced a meaningfully large cache");
    for (key, seq_style) in &sequential {
        assert_eq!(parallel.get(key), Some(seq_style));
    }
}

/// Security regression: without `MAX_STYLE_DEPTH`,
/// `resolve_subtree_parallel` recurses once per DOM level with no
/// bound, and a tree built directly (bypassing
/// `html_parser::MAX_NESTING_DEPTH` — the same shape a JS
/// `appendChild` loop could produce) can overflow the native/
/// rayon-worker stack. This must complete without crashing, and
/// elements past the cap simply get no cache entry (which
/// `layout::style_of` already treats as `Style::default()`).
#[test]
fn resolve_style_tree_on_an_extremely_deep_tree_does_not_overflow_the_stack() {
    let depth = 20_000;
    let mut leaf = Element::new("div");
    leaf.attributes.push(("id".into(), "innermost".into()));
    let mut root = leaf.clone();
    for _ in 0..depth {
        let mut parent = Element::new("div");
        parent.children.push(dom::Node::Element(root));
        root = parent;
    }
    let cache = resolve_style_tree_parallel(&root, &Stylesheet::default(), DEFAULT_VIEWPORT_WIDTH);
    assert!(cache.len() <= MAX_STYLE_DEPTH + 4, "cache should be bounded by the depth cap, not the full {depth}");
}

#[test]
fn parallel_resolution_respects_inheritance_across_levels() {
    let mut root = Element::new("div");
    root.attributes.push(("style".into(), "color:#123456".into()));
    let mut mid = Element::new("div");
    let mut leaf = Element::new("span");
    leaf.children.push(dom::Node::Element(Element::new("i")));
    mid.children.push(dom::Node::Element(leaf));
    root.children.push(dom::Node::Element(mid));

    let cache = resolve_style_tree_parallel(&root, &Stylesheet::default(), DEFAULT_VIEWPORT_WIDTH);
    // Every descendant should have inherited `color` from the root,
    // three levels down — proves the parallel path threads
    // `parent_style` through correctly, not just per-level.
    for (_, style) in &cache {
        assert_eq!(style.color, Color { r: 0x12, g: 0x34, b: 0x56, a: 255 });
    }
}

#[test]
fn parallel_resolution_on_empty_tree_does_not_panic() {
    let root = Element::new("div");
    let cache = resolve_style_tree_parallel(&root, &Stylesheet::default(), DEFAULT_VIEWPORT_WIDTH);
    assert_eq!(cache.len(), 1); // just the root itself
}

#[test]
fn different_elements_get_distinct_cache_entries_even_with_identical_content() {
    let mut root = Element::new("div");
    // Two structurally-identical (but distinct) elements — must
    // not collide in the cache despite having the same content.
    root.children.push(dom::Node::Element(Element::new("span")));
    root.children.push(dom::Node::Element(Element::new("span")));

    let cache = resolve_style_tree_parallel(&root, &Stylesheet::default(), DEFAULT_VIEWPORT_WIDTH);
    assert_eq!(cache.len(), 3); // root + 2 distinct spans
}

// ---- Phase A: selectors & cascade completeness ----

fn el_with(tag: &str, attrs: &[(&str, &str)]) -> Element {
    let mut e = Element::new(tag);
    for (k, v) in attrs {
        e.attributes.push(((*k).into(), v.to_string()));
    }
    e
}

/// Builds `<root>` with the given children, resolves the whole tree
/// at `viewport`, and returns each child's resolved `Style` in order.
fn resolve_children(root_tag: &str, children: Vec<Element>, css: &str, viewport: f32) -> Vec<Style> {
    let mut root = Element::new(root_tag);
    for c in children {
        root.children.push(dom::Node::Element(c));
    }
    // Keys must be derived AFTER the elements are in place — the key
    // is the element's address, which moves when it's pushed.
    let keys: Vec<dom::NodeId> = root
        .children
        .iter()
        .filter_map(|n| match n {
            dom::Node::Element(e) => Some(e.node_id),
            _ => None,
        })
        .collect();
    let sheet = parse_stylesheet(css);
    let cache = resolve_style_tree_parallel(&root, &sheet, viewport);
    keys.iter().map(|k| cache.get(k).cloned().unwrap()).collect()
}

#[test]
fn attribute_selectors_match() {
    let styles = resolve_children(
        "div",
        vec![
            el_with("a", &[("href", "https://example.com/x.pdf")]),
            el_with("a", &[("href", "/local")]),
            el_with("input", &[("type", "text")]),
        ],
        r#"
          a[href^="https"] { color: #ff0000; }
          a[href$=".pdf"] { background-color: #00ff00; }
          [type="text"] { color: #0000ff; }
        "#,
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(styles[0].background_color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(styles[1].color, Color { r: 0, g: 0, b: 0, a: 255 }); // no https, no .pdf
    assert_eq!(styles[2].color, Color { r: 0, g: 0, b: 255, a: 255 });
}

#[test]
fn attribute_includes_and_dashmatch() {
    let styles = resolve_children(
        "div",
        vec![el_with("p", &[("class", "a b c"), ("lang", "en-US")])],
        "[class~=\"b\"] { color: #ff0000; } [lang|=\"en\"] { background-color: #00ff00; }",
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(styles[0].background_color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn child_combinator_only_matches_direct_children() {
    // div > p should match a direct <p> child but not a <p> nested
    // inside a <section>.
    let mut section = Element::new("section");
    section.children.push(dom::Node::Element(Element::new("p")));
    let mut root = Element::new("div");
    root.children.push(dom::Node::Element(Element::new("p")));
    root.children.push(dom::Node::Element(section));

    let direct_key = match &root.children[0] {
        dom::Node::Element(e) => e.node_id,
        _ => unreachable!(),
    };
    let nested_key = {
        let dom::Node::Element(s) = &root.children[1] else { unreachable!() };
        let dom::Node::Element(n) = &s.children[0] else { unreachable!() };
        n.node_id
    };

    let sheet = parse_stylesheet("div > p { color: #ff0000; }");
    let cache = resolve_style_tree_parallel(&root, &sheet, 1000.0);
    assert_eq!(cache[&direct_key].color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(cache[&nested_key].color, Color { r: 0, g: 0, b: 0, a: 255 });
}

#[test]
fn next_sibling_and_subsequent_sibling_combinators() {
    let styles = resolve_children(
        "ul",
        vec![
            el_with("li", &[("id", "a")]),
            el_with("li", &[("id", "b")]),
            el_with("li", &[("id", "c")]),
        ],
        "li + li { color: #ff0000; } #a ~ li { background-color: #00ff00; }",
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 0, g: 0, b: 0, a: 255 }); // first li: no preceding li
    assert_eq!(styles[1].color, Color { r: 255, g: 0, b: 0, a: 255 }); // b follows a
    assert_eq!(styles[2].color, Color { r: 255, g: 0, b: 0, a: 255 }); // c follows b
    assert_eq!(styles[1].background_color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(styles[2].background_color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(styles[0].background_color, Color::TRANSPARENT);
}

#[test]
fn structural_pseudo_classes() {
    let styles = resolve_children(
        "ul",
        vec![Element::new("li"), Element::new("li"), Element::new("li"), Element::new("li")],
        "li:first-child { color: #ff0000; } li:last-child { color: #00ff00; } li:nth-child(2n) { background-color: #0000ff; }",
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(styles[3].color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(styles[1].background_color, Color { r: 0, g: 0, b: 255, a: 255 }); // 2nd
    assert_eq!(styles[3].background_color, Color { r: 0, g: 0, b: 255, a: 255 }); // 4th
    assert_eq!(styles[2].background_color, Color::TRANSPARENT); // 3rd
}

#[test]
fn only_child_and_not() {
    let solo = resolve_children("div", vec![Element::new("span")], "span:only-child { color: #ff0000; }", 1000.0);
    assert_eq!(solo[0].color, Color { r: 255, g: 0, b: 0, a: 255 });

    let styles = resolve_children(
        "div",
        vec![el_with("p", &[("class", "keep")]), el_with("p", &[("class", "skip")])],
        "p:not(.skip) { color: #00ff00; }",
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(styles[1].color, Color { r: 0, g: 0, b: 0, a: 255 });
}

#[test]
fn interactive_pseudo_classes_never_match_but_do_not_break_the_rule() {
    let styles = resolve_children("div", vec![Element::new("a")], "a:hover { color: #ff0000; }", 1000.0);
    assert_eq!(styles[0].color, Color { r: 0, g: 0, b: 0, a: 255 });
}

#[test]
fn required_empty_input_matches_invalid_not_valid() {
    let input = el_with("input", &[("required", "required")]);
    let css = "input:invalid { color: #ff0000; } input:valid { color: #00ff00; }";
    let styles = resolve_children("div", vec![input], css, 1000.0);
    assert_eq!(styles[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn required_filled_input_matches_valid_not_invalid() {
    let input = el_with("input", &[("required", "required"), ("value", "hi")]);
    let css = "input:invalid { color: #ff0000; } input:valid { color: #00ff00; }";
    let styles = resolve_children("div", vec![input], css, 1000.0);
    assert_eq!(styles[0].color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn pattern_mismatch_matches_invalid() {
    let input = el_with("input", &[("pattern", "[0-9]+"), ("value", "not-digits")]);
    let css = "input:invalid { color: #ff0000; }";
    let styles = resolve_children("div", vec![input], css, 1000.0);
    assert_eq!(styles[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn pattern_match_matches_valid() {
    let input = el_with("input", &[("pattern", "[0-9]+"), ("value", "12345")]);
    let css = "input:invalid { color: #ff0000; } input:valid { color: #00ff00; }";
    let styles = resolve_children("div", vec![input], css, 1000.0);
    assert_eq!(styles[0].color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn a_non_input_element_is_always_valid_never_invalid() {
    let div = Element::new("div");
    let css = "div:invalid { color: #ff0000; } div:valid { color: #00ff00; }";
    let styles = resolve_children("body", vec![div], css, 1000.0);
    assert_eq!(styles[0].color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn pseudo_element_rule_does_not_apply_to_the_real_element() {
    let styles = resolve_children("div", vec![Element::new("p")], "p::before { color: #ff0000; content: 'x'; }", 1000.0);
    assert_eq!(styles[0].color, Color { r: 0, g: 0, b: 0, a: 255 });
    assert_eq!(styles[0].content, None);
}

#[test]
fn media_query_gates_rules_on_viewport_width() {
    let css = "p { color: #ff0000; } @media (max-width: 600px) { p { color: #00ff00; } }";
    let wide = resolve_children("div", vec![Element::new("p")], css, 1000.0);
    assert_eq!(wide[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
    let narrow = resolve_children("div", vec![Element::new("p")], css, 500.0);
    assert_eq!(narrow[0].color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn media_min_and_max_range() {
    let css = "@media (min-width: 400px) and (max-width: 800px) { p { color: #ff0000; } }";
    assert_eq!(resolve_children("div", vec![Element::new("p")], css, 300.0)[0].color, Color { r: 0, g: 0, b: 0, a: 255 });
    assert_eq!(resolve_children("div", vec![Element::new("p")], css, 600.0)[0].color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(resolve_children("div", vec![Element::new("p")], css, 900.0)[0].color, Color { r: 0, g: 0, b: 0, a: 255 });
}

#[test]
fn custom_properties_resolve_and_inherit() {
    let mut root = Element::new("div");
    root.attributes.push(("style".into(), "--brand: #ff0000".into()));
    let mut child = Element::new("p");
    child.attributes.push(("style".into(), "color: var(--brand)".into()));
    root.children.push(dom::Node::Element(child));
    let child_key = match &root.children[0] {
        dom::Node::Element(e) => e.node_id,
        _ => unreachable!(),
    };

    let cache = resolve_style_tree_parallel(&root, &Stylesheet::default(), 1000.0);
    assert_eq!(cache[&child_key].color, Color { r: 255, g: 0, b: 0, a: 255 });
}

#[test]
fn var_fallback_used_when_property_missing() {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), "color: var(--nope, #00ff00)".into()));
    let style = resolve_style(&el, &[], None, &Stylesheet::default());
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn custom_property_from_stylesheet_is_visible_to_var_in_lower_priority_rule() {
    // `--c` is set by a higher-specificity rule; a `var(--c)` in a
    // lower one still sees it (custom props cascade first).
    let styles = resolve_children(
        "div",
        vec![el_with("p", &[("id", "x"), ("class", "y")])],
        ".y { color: var(--c); } #x { --c: #0000ff; }",
        1000.0,
    );
    assert_eq!(styles[0].color, Color { r: 0, g: 0, b: 255, a: 255 });
}

#[test]
fn parallel_resolution_still_matches_sequential_with_combinators_in_the_sheet() {
    // Guards the concurrency invariant the plan calls out: adding
    // sibling/pseudo matching must not desync the parallel path.
    // (Sequential reference here is a plain recursive walk using the
    // same context-aware resolver.)
    let sheet = parse_stylesheet(
        "div > p:first-child { color: #ff0000; } p + p { background-color: #00ff00; } .item-2 { font-size: 30px; }",
    );
    let tree = build_synthetic_tree(4, 4);
    let parallel = resolve_style_tree_parallel(&tree, &sheet, 1000.0);
    let sequential = resolve_sequential_ctx(&tree, &sheet, 1000.0);
    assert_eq!(parallel.len(), sequential.len());
    for (k, v) in &sequential {
        assert_eq!(parallel.get(k), Some(v), "mismatch at {k}");
    }
}

fn resolve_sequential_ctx(root: &Element, sheet: &Stylesheet, vw: f32) -> StyleCache {
    fn go(
        el: dom::ElementRef<'_>,
        ancestors: &[dom::ElementRef<'_>],
        parent_style: Option<&Style>,
        prev: &[dom::ElementRef<'_>],
        idx: usize,
        count: usize,
        sheet: &Stylesheet,
        vw: f32,
        out: &mut StyleCache,
    ) {
        let ctx = MatchContext {
            ancestors,
            parent_style,
            previous_siblings: prev,
            next_sibling_exists: idx + 1 < count,
            sibling_index: idx,
            sibling_count: count,
            viewport_width: vw,
            viewport_height: crate::DEFAULT_VIEWPORT_HEIGHT,
            interaction: crate::Interaction::none(),
        };
        let style = resolve_style_in_context(el, &ctx, sheet);
        out.insert(el.node_id(), style.clone());
        let mut child_ancestors = vec![el];
        child_ancestors.extend_from_slice(ancestors);
        let kids: Vec<dom::ElementRef<'_>> = el.element_children().collect();
        for i in 0..kids.len() {
            go(kids[i], &child_ancestors, Some(&style), &kids[..i], i, kids.len(), sheet, vw, out);
        }
    }
    let mut out = StyleCache::default();
    go(root.into(), &[], None, &[], 0, 1, sheet, vw, &mut out);
    out
}

// ---- Phase C: box model & paint completeness ----

fn style_of_inline(css: &str) -> Style {
    let mut el = Element::new("div");
    el.attributes.push(("style".into(), css.into()));
    resolve_style(&el, &[], None, &Stylesheet::default())
}

/// M6's gap: each side has its own width, style and colour, set by the
/// side shorthands and longhands and by the one-to-four-value box forms.
#[test]
fn border_sides_are_independent() {
    let red = Color { r: 255, g: 0, b: 0, a: 255 };
    let s = style_of_inline("border-bottom: 2px dashed red");
    assert_eq!((s.border().bottom.width, s.border().bottom.style, s.border().bottom.color), (2.0, BorderStyle::Dashed, red));
    assert_eq!(s.border().top.style, BorderStyle::None, "other sides untouched");

    let s = style_of_inline("border: 1px solid red; border-top: none; border-left-width: 5px; border-right-color: blue");
    assert_eq!(s.border().top.style, BorderStyle::None, "a side shorthand resets its side");
    assert_eq!((s.border().left.width, s.border().left.style), (5.0, BorderStyle::Solid));
    assert_eq!(s.border().right.color, Color { r: 0, g: 0, b: 255, a: 255 });
    assert_eq!(s.border().bottom.width, 1.0);

    let s = style_of_inline("border-width: 1px 2px 3px; border-style: solid none; border-color: red");
    assert_eq!((s.border().top.width, s.border().right.width, s.border().bottom.width, s.border().left.width), (1.0, 2.0, 3.0, 2.0), "three values: left = right");
    assert_eq!((s.border().top.style, s.border().right.style, s.border().bottom.style, s.border().left.style), (BorderStyle::Solid, BorderStyle::None, BorderStyle::Solid, BorderStyle::None));
    assert_eq!(s.border().used_widths(), EdgeSizes { top: 1.0, right: 0.0, bottom: 3.0, left: 0.0 }, "no style, no width");

    let s = style_of_inline("border-width: 1px 2px 3px 4px 5px");
    assert_eq!(s.border().top.width, 0.0, "five values are invalid: ignored");
    let s = style_of_inline("color: lime; border-bottom: 1px solid");
    assert_eq!(s.border().bottom.color, Color { r: 0, g: 255, b: 0, a: 255 }, "an omitted colour is currentcolor");
}

#[test]
fn border_style_variants_parse() {
    assert_eq!(style_of_inline("border-style: dashed").border().top.style, BorderStyle::Dashed);
    assert_eq!(style_of_inline("border-style: dotted").border().top.style, BorderStyle::Dotted);
    assert_eq!(style_of_inline("border-style: double").border().top.style, BorderStyle::Double);
    assert_eq!(style_of_inline("border: 4px dashed red").border().top.style, BorderStyle::Dashed);
    assert_eq!(style_of_inline("border: 4px dashed red").border().top.width, 4.0);
}

#[test]
fn border_radius_parses_first_length() {
    assert_eq!(style_of_inline("border-radius: 8px").border_radius, 8.0);
    assert_eq!(style_of_inline("border-radius: 8px 4px").border_radius, 8.0); // first value only
    assert_eq!(style_of_inline("border-radius: -3px").border_radius, 0.0); // clamped
}

#[test]
fn outline_longhands_and_shorthand_parse() {
    let s = style_of_inline("outline-width: 2px; outline-style: solid; outline-color: red; outline-offset: 3px");
    assert_eq!(s.outline_width, 2.0);
    assert_eq!(s.outline_style, BorderStyle::Solid);
    assert_eq!(s.outline_color, Color { r: 255, g: 0, b: 0, a: 255 });
    assert_eq!(s.outline_offset, 3.0);

    let sh = style_of_inline("outline: 3px dotted #00ff00");
    assert_eq!(sh.outline_width, 3.0);
    assert_eq!(sh.outline_style, BorderStyle::Dotted);
    assert_eq!(sh.outline_color, Color { r: 0, g: 255, b: 0, a: 255 });
}

#[test]
fn outline_and_border_radius_do_not_inherit() {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), "outline: 2px solid red; border-radius: 10px".into()));
    let ps = resolve_style(&parent, &[], None, &Stylesheet::default());
    let child = Element::new("div");
    let cs = resolve_style(&child, &[&parent], Some(&ps), &Stylesheet::default());
    assert_eq!(cs.outline_style, BorderStyle::None);
    assert_eq!(cs.border_radius, 0.0);
}

// ---- Phase D / E: new style properties ----

#[test]
fn flex_wrap_align_content_and_order_parse() {
    let s = style_of_inline("flex-wrap: wrap-reverse; align-content: space-between; order: -2");
    assert_eq!(s.flex_wrap, FlexWrap::WrapReverse);
    assert_eq!(s.align_content, AlignContent::SpaceBetween);
    assert_eq!(s.order, -2);
}

#[test]
fn intrinsic_width_keywords_parse_and_clear_the_length() {
    assert_eq!(style_of_inline("width: min-content").width_intrinsic, Some(IntrinsicKeyword::MinContent));
    assert_eq!(style_of_inline("width: max-content").width_intrinsic, Some(IntrinsicKeyword::MaxContent));
    let f = style_of_inline("width: fit-content");
    assert_eq!(f.width_intrinsic, Some(IntrinsicKeyword::FitContent));
    assert_eq!(f.width, None);
    // A plain length leaves the keyword unset.
    assert_eq!(style_of_inline("width: 40px").width_intrinsic, None);
}

#[test]
fn object_fit_parses() {
    assert_eq!(style_of_inline("object-fit: contain").object_fit, ObjectFit::Contain);
    assert_eq!(style_of_inline("object-fit: cover").object_fit, ObjectFit::Cover);
    assert_eq!(style_of_inline("object-fit: scale-down").object_fit, ObjectFit::ScaleDown);
}

#[test]
fn background_size_and_position_parse() {
    assert_eq!(style_of_inline("background-size: cover").background_size, BackgroundSize::Cover);
    assert_eq!(style_of_inline("background-size: 100px 50px").background_size, BackgroundSize::Explicit(Some(Length::px(100.0)), Some(Length::px(50.0))));
    assert_eq!(style_of_inline("background-size: 80px").background_size, BackgroundSize::Explicit(Some(Length::px(80.0)), None));
    assert_eq!(style_of_inline("background-size: auto 50%").background_size, BackgroundSize::Explicit(None, Some(Length::percent(50.0))));

    let pos = |css: &str| {
        let p = style_of_inline(&format!("background-position: {css}")).background_position;
        (p.x, p.y)
    };
    let pc = Length::percent;
    assert_eq!(pos("center center"), (pc(50.0), pc(50.0)));
    assert_eq!(pos("right bottom"), (pc(100.0), pc(100.0)));
    assert_eq!(pos("25% 75%"), (pc(25.0), pc(75.0)));
    assert_eq!(pos("top left"), (pc(0.0), pc(0.0)), "keywords in either order");
    assert_eq!(pos("center left"), (pc(0.0), pc(50.0)));
    assert_eq!(pos("top"), (pc(50.0), pc(0.0)));
    assert_eq!(pos("10px 2em"), (Length::px(10.0), Length::px(32.0)));
    assert_eq!(pos("right 10px bottom 5%"), (Length { px: -10.0, percent: 100.0 }, pc(95.0)));
    assert_eq!(pos("left 4px top"), (Length::px(4.0), pc(0.0)));
    let unchanged = style_of_inline("background-position: 5px 5px; background-position: left left").background_position;
    assert_eq!(unchanged.x, Length::px(5.0), "an invalid position is ignored");
}

#[test]
fn background_repeat_parses() {
    use crate::BackgroundRepeat as R;
    for (css, r) in [("no-repeat", R::NoRepeat), ("repeat-x", R::RepeatX), ("repeat-y", R::RepeatY), ("repeat no-repeat", R::RepeatX), ("space", R::Repeat), ("no-repeat repeat", R::RepeatY)] {
        assert_eq!(style_of_inline(&format!("background-repeat: {css}")).background_repeat, r, "{css}");
    }
}

#[test]
fn background_shorthand_sets_every_part_and_resets_the_rest() {
    use crate::BackgroundRepeat as R;
    let s = style_of_inline("background: #102030");
    assert_eq!(s.background_color, Color { r: 0x10, g: 0x20, b: 0x30, a: 255 });
    let s = style_of_inline("background: url(a.png) no-repeat right 5px top / 20px auto rgb(1, 2, 3)");
    assert_eq!(s.background_image_path.as_deref(), Some("a.png"));
    assert_eq!(s.background_repeat, R::NoRepeat);
    assert_eq!(s.background_position.x, Length { px: -5.0, percent: 100.0 });
    assert_eq!(s.background_size, BackgroundSize::Explicit(Some(Length::px(20.0)), None));
    assert_eq!(s.background_color, Color { r: 1, g: 2, b: 3, a: 255 });
    let s = style_of_inline("background: center/cover url('b.jpg')");
    assert_eq!((s.background_size, s.background_position.x), (BackgroundSize::Cover, Length::percent(50.0)));
    // The shorthand resets what it does not name.
    let s = style_of_inline("background-color: red; background-repeat: no-repeat; background: url(c.png)");
    assert_eq!((s.background_color, s.background_repeat), (Color::TRANSPARENT, R::Repeat));
    // Several layers: the first image, the last layer's colour.
    let s = style_of_inline("background: url(top.png) no-repeat, url(bottom.png), white");
    assert_eq!(s.background_image_path.as_deref(), Some("top.png"));
    assert_eq!(s.background_color, Color { r: 255, g: 255, b: 255, a: 255 });
    // Invalid: a colour in a non-final layer, or an unknown token — ignored.
    let s = style_of_inline("background: blue; background: red, url(x.png)");
    assert_eq!(s.background_color, Color { r: 0, g: 0, b: 255, a: 255 });
    let s = style_of_inline("background: blue; background: red wobbly");
    assert_eq!(s.background_color, Color { r: 0, g: 0, b: 255, a: 255 });
    // A gradient is not an image this engine draws: the colour still applies.
    let s = style_of_inline("background: linear-gradient(red, blue) #00ff00");
    assert_eq!((s.background_image_path.as_deref(), s.background_color), (None, Color { r: 0, g: 255, b: 0, a: 255 }));
}

// ---- Milestone 25 (found by the fuzz harness): unbounded font-size ----

#[test]
fn oversized_font_size_is_clamped_not_passed_through() {
    // 99999999px used to reach glyph rasterization and abort the
    // process on a multi-petabyte allocation.
    assert_eq!(style_of_inline("font-size: 99999999px").font_size, MAX_FONT_SIZE_PX);
    assert_eq!(style_of_inline("font-size: 1024px").font_size, 1024.0);
    assert_eq!(style_of_inline("font-size: 24px").font_size, 24.0);
}

#[test]
fn non_finite_or_negative_font_size_is_ignored_like_any_invalid_value() {
    let default = style_of_inline("").font_size;
    for bad in ["font-size: 1e999px", "font-size: -1e999px", "font-size: nanpx", "font-size: infpx", "font-size: -5px"] {
        assert_eq!(style_of_inline(bad).font_size, default, "{bad} should be ignored");
    }
}

#[test]
fn font_size_is_always_finite_and_bounded_for_hostile_stylesheets() {
    let mut el = Element::new("p");
    el.attributes.push(("class".into(), "x".into()));
    let sheet = css_parser::parse_stylesheet(".x{font-size:1e38px} p.x{font-size:340282350000000000000000000000000000000px}");
    let s = resolve_style(&el, &[], None, &sheet);
    assert!(s.font_size.is_finite() && s.font_size <= MAX_FONT_SIZE_PX);
}

// ---- Milestone 27: every silent cap is observable ----

#[test]
fn font_size_clamping_and_rejection_each_report_a_guard_event() {
    use diagnostics::{collector::capture, Guard};
    let (_, c) = capture(|| style_of_inline("font-size: 99999999px"));
    assert!(c.saw_guard(Guard::FontSizeClamped), "an oversized font-size is clamped, visibly");
    let (_, c) = capture(|| style_of_inline("font-size: 1e999px"));
    assert!(c.saw_guard(Guard::FontSizeClamped), "a non-finite font-size is ignored, visibly");
    let (_, c) = capture(|| style_of_inline("font-size: 24px"));
    assert!(c.events.is_empty(), "an ordinary font-size trips nothing: {:?}", c.events);
}

#[test]
fn cascading_a_tree_past_the_style_depth_cap_bumps_the_guard_counter() {
    // Style resolution walks in `rayon` workers, so the EVENT may be
    // emitted off the test thread; the process-global counter is not
    // thread-local. Deltas only: other tests bump it too.
    let before = diagnostics::metrics::snapshot();
    let mut root = Element::new("div");
    for _ in 0..(MAX_STYLE_DEPTH + 50) {
        let mut parent = Element::new("div");
        parent.children.push(dom::Node::Element(root));
        root = parent;
    }
    let _ = resolve_style_tree_parallel(&root, &Stylesheet::default(), DEFAULT_VIEWPORT_WIDTH);
    assert!(diagnostics::metrics::snapshot().delta(&before, "guard.style_depth") >= 1);
}

// ---- Hardening pass: a hostile `pattern` cannot stall the cascade ----

fn input_with_pattern(pattern: &str, value: &str) -> Element {
    let mut root = Element::new("div");
    let mut input = Element::new("input");
    input.attributes.push(("id".into(), "i".into()));
    input.attributes.push(("pattern".into(), pattern.into()));
    input.attributes.push(("value".into(), value.into()));
    root.children.push(dom::Node::Element(input));
    root
}

#[test]
fn a_catastrophic_pattern_does_not_stall_the_cascade() {
    // Measured before the fix (the principles audit): 24 chars = 0.8 s,
    // 28 chars = 4.7 s — re-evaluated on EVERY frame, so a permanent freeze.
    let root = input_with_pattern("(a+)+$", &format!("{}!", "a".repeat(5000)));
    let sheet = css_parser::parse_stylesheet("input:invalid { color: #ff0000 }");
    let started = std::time::Instant::now();
    let cache = resolve_style_tree_parallel(&root, &sheet, DEFAULT_VIEWPORT_WIDTH);
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "cascade took {:?}", started.elapsed());

    let style = cache.get(&(root.find_by_id("i").unwrap()).node_id).unwrap();
    assert_eq!(style.color, Color { r: 255, g: 0, b: 0, a: 255 }, "the value does not match the pattern, so :invalid still applies");
}

#[test]
fn an_unusable_pattern_imposes_no_constraint() {
    let root = input_with_pattern(r"(a)\1", "zzz");
    let sheet = css_parser::parse_stylesheet("input:invalid { color: #ff0000 } input:valid { color: #00ff00 }");
    let cache = resolve_style_tree_parallel(&root, &sheet, DEFAULT_VIEWPORT_WIDTH);
    let style = cache.get(&(root.find_by_id("i").unwrap()).node_id).unwrap();
    assert_eq!(style.color, Color { r: 0, g: 255, b: 0, a: 255 }, "a backreference pattern is ignored (valid), not evaluated");
}

// ---- Milestone 33: units, font-size inheritance and ordering ----

fn child_style(parent_css: &str, child_css: &str) -> Style {
    let mut parent = Element::new("div");
    parent.attributes.push(("style".into(), parent_css.into()));
    let mut child = Element::new("p");
    child.attributes.push(("style".into(), child_css.into()));
    let parent_style = resolve_style(&parent, &[], None, &Stylesheet::default());
    resolve_style(&child, &[&parent], Some(&parent_style), &Stylesheet::default())
}

#[test]
fn font_size_inherits_and_em_is_relative_to_the_parent() {
    assert_eq!(child_style("font-size: 20px", "").font_size, 20.0, "font-size inherits");
    assert_eq!(child_style("font-size: 20px", "font-size: 1.5em").font_size, 30.0);
    assert_eq!(child_style("font-size: 20px", "font-size: 50%").font_size, 10.0);
    assert_eq!(child_style("font-size: 20px", "font-size: 2rem").font_size, 40.0, "rem is the root's size");
    assert_eq!(child_style("font-size: 20px", "font-size: larger").font_size, 24.0);
    assert_eq!(child_style("font-size: 20px", "font-size: x-large").font_size, 24.0);
    assert_eq!(child_style("", "font-size: 12pt").font_size, 16.0);
}

#[test]
fn font_size_applies_before_other_em_lengths_whatever_the_declaration_order() {
    let s = child_style("font-size: 10px", "padding-left: 2em; font-size: 20px");
    assert_eq!(s.padding.left, Length::px(40.0), "em in padding uses the element's own final font size");
    // Several font-size declarations: each is relative to the parent, and
    // the last valid one wins — not compounded.
    let mut el = Element::new("p");
    el.attributes.push(("class".into(), "a".into()));
    el.attributes.push(("style".into(), "font-size: 2em".into()));
    let sheet = css_parser::parse_stylesheet(".a { font-size: 3em }");
    assert_eq!(resolve_style(&el, &[], None, &sheet).font_size, 32.0);
}

#[test]
fn lengths_accept_every_unit_and_calc() {
    let s = style_of_inline("width: calc(50% + 1em); height: 2rem; margin: 0 auto; padding: 1pt 0");
    assert_eq!(s.width, Some(Length { px: 16.0, percent: 50.0 }));
    assert_eq!(s.height, Some(Length::px(32.0)));
    assert_eq!(s.margin.left, None, "auto");
    assert_eq!(s.margin.top, Some(Length::ZERO), "a unitless zero is a length");
    assert_eq!(s.padding.top.px, 96.0 / 72.0);
    let s = style_of_inline("border: thin solid #010203");
    assert_eq!((s.border().top.width, s.border().top.color), (1.0, Color { r: 1, g: 2, b: 3, a: 255 }));
    assert_eq!(style_of_inline("border: calc(1px + 1px) solid").border().top.width, 2.0, "a calc() with spaces is one value");
}

#[test]
fn invalid_lengths_leave_the_previous_value_in_force() {
    let s = style_of_inline("width: 10px; width: -5px; padding: 3px; padding: -1px; margin: 4px; margin: 1px 2px 3px 4px 5px");
    assert_eq!(s.width, Some(Length::px(10.0)));
    assert_eq!(s.padding, crate::Edges::all(Length::px(3.0)));
    assert_eq!(s.margin, crate::Edges::all(Some(Length::px(4.0))));
    let s = style_of_inline("width: calc(100% - 20px)");
    assert_eq!(s.width, Some(Length { px: -20.0, percent: 100.0 }), "a mixed-sign calc() is valid; layout clamps it");
}

#[test]
fn viewport_units_follow_the_viewport_and_a_resize_restyles() {
    use crate::{StyleEngine, ViewportSize};
    let mut root = Element::new("div");
    root.attributes.push(("style".into(), "height: 50vh".into()));
    let doc = dom::Document::new(root);
    let mut engine = StyleEngine::new();
    let sheet = Stylesheet::default();
    engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &dom::Mutations::default());
    assert_eq!(engine.styles()[&doc.root().node_id()].height, Some(Length::px(300.0)));
    let report = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 400.0), &dom::Mutations::default());
    assert!(report.full, "an inline vh makes the resize a full restyle");
    assert_eq!(engine.styles()[&doc.root().node_id()].height, Some(Length::px(200.0)));
}

#[test]
fn width_transitions_interpolate_percent_and_px_separately() {
    let el = Element::new("div");
    let mut style = Style {
        transitions: vec![TransitionSpec { property: TransitionProperty::Width, duration: 1.0, easing: Easing::Linear }],
        width: Some(Length::percent(50.0)),
        ..Style::default()
    };
    let mut registry = TransitionRegistry::new();
    let start = std::time::Instant::now();
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start);
    style.width = Some(Length::px(100.0));
    apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start); // starts the transition
    let displayed = apply_transitions(&el, cache_with(&el, style.clone()), &mut registry, start + std::time::Duration::from_millis(500));
    assert_eq!(style_in(&displayed, &el).width, Some(Length { px: 50.0, percent: 25.0 }), "calc(25% + 50px), halfway");
}

#[test]
fn currentcolor_is_the_final_color_and_the_initial_border_color() {
    let s = style_of_inline("border-top-color: red; background-color: currentcolor; color: rgb(0, 0, 255)");
    assert_eq!(s.background_color, Color { r: 0, g: 0, b: 255, a: 255 }, "resolved against the final colour, whatever the order");
    let s = style_of_inline("color: #00ff00; border: 1px solid");
    assert_eq!(s.border().top.color, Color { r: 0, g: 255, b: 0, a: 255 }, "border-color starts as currentcolor");
    assert_eq!(child_style("color: teal", "color: currentcolor").color, Color { r: 0, g: 128, b: 128, a: 255 });
}

#[test]
fn root_pseudo_class_matches_only_the_element_without_a_parent() {
    let sheet = css_parser::parse_stylesheet(":root { width: 1px } :root > p { width: 2px } :root p.deep { width: 3px } p:not(:root) { height: 4px }");
    let html = Element::new("html");
    let mut p = Element::new("p");
    p.attributes.push(("class".into(), "deep".into()));
    assert_eq!(resolve_style(&html, &[], None, &sheet).width, Some(Length::px(1.0)));
    let child = resolve_style(&p, &[&html], None, &sheet);
    assert_eq!(child.width, Some(Length::px(3.0)), ":root > p and :root p.deep both match; the more specific wins");
    assert_eq!(child.height, Some(Length::px(4.0)));
    let body = Element::new("body");
    assert_eq!(resolve_style(&p, &[&body, &html], None, &sheet).width, Some(Length::px(3.0)));
    assert_eq!(resolve_style(&p, &[&body, &html], None, &css_parser::parse_stylesheet(":root > p { width: 2px }")).width, None);
}

#[test]
fn display_values_and_tag_defaults() {
    use crate::Display;
    for (css, d) in [("none", Display::None), ("inline", Display::Inline), ("INLINE-BLOCK", Display::InlineBlock), ("inline-flex", Display::InlineFlex), ("flow-root", Display::Block), ("list-item", Display::Block)] {
        assert_eq!(style_of_inline(&format!("display: {css}")).display, d, "{css}");
    }
    let styled = |tag: &str, attrs: &[(&str, &str)]| {
        let mut el = Element::new(tag);
        for (k, v) in attrs {
            el.attributes.push(((*k).into(), (*v).into()));
        }
        resolve_style(&el, &[], None, &Stylesheet::default()).display
    };
    assert_eq!(styled("span", &[]), Display::Inline);
    assert_eq!(styled("img", &[]), Display::InlineBlock);
    assert_eq!(styled("script", &[]), Display::None);
    assert_eq!(styled("div", &[("hidden", "")]), Display::None);
    assert_eq!(styled("input", &[("type", "HIDDEN")]), Display::None);
    assert_eq!(styled("section", &[]), Display::Block);
    let mut el = Element::new("span");
    el.attributes.push(("style".into(), "display: ruby-text-container".into()));
    assert_eq!(resolve_style(&el, &[], None, &Stylesheet::default()).display, Display::Inline, "an unsupported value leaves the default");
}

#[test]
fn overflow_parses_per_axis_and_fixes_up_visible() {
    use crate::Overflow as O;
    let s = style_of_inline("overflow: hidden");
    assert_eq!((s.overflow_x, s.overflow_y), (O::Hidden, O::Hidden));
    let s = style_of_inline("overflow: clip auto");
    assert_eq!((s.overflow_x, s.overflow_y), (O::Hidden, O::Auto), "clip next to a scrollable axis becomes hidden");
    let s = style_of_inline("overflow-y: scroll");
    assert_eq!((s.overflow_x, s.overflow_y), (O::Auto, O::Scroll), "visible next to a scrollable axis becomes auto");
    let s = style_of_inline("overflow: clip");
    assert_eq!((s.overflow_x, s.overflow_y), (O::Clip, O::Clip));
    assert_eq!(style_of_inline("overflow: bogus").overflow_x, O::Visible);
}

#[test]
fn opacity_and_visibility_parse_and_visibility_inherits() {
    assert_eq!(style_of_inline("opacity: 0.25").opacity, 0.25);
    assert_eq!(style_of_inline("opacity: 50%").opacity, 0.5);
    assert_eq!(style_of_inline("opacity: 7").opacity, 1.0, "clamped");
    assert_eq!(style_of_inline("opacity: nan").opacity, 1.0);
    assert_eq!(child_style("visibility: hidden", "").visibility, crate::Visibility::Hidden);
    assert_eq!(child_style("visibility: hidden", "visibility: visible").visibility, crate::Visibility::Visible);
    assert_eq!(child_style("opacity: 0.5", "").opacity, 1.0, "opacity does not inherit");
}

#[test]
fn box_shadow_parses_lists_in_any_order_and_rejects_invalid() {
    let s = style_of_inline("color: #010203; box-shadow: 1px 2px 3px 4px red, inset 0 0 5px, #00ff00 2px 2px");
    assert_eq!(s.box_shadow.len(), 3);
    let a = s.box_shadow[0];
    assert_eq!((a.offset_x, a.offset_y, a.blur, a.spread, a.color.r, a.inset), (1.0, 2.0, 3.0, 4.0, 255, false));
    assert!(s.box_shadow[1].inset);
    assert_eq!(s.box_shadow[1].color, Color { r: 1, g: 2, b: 3, a: 255 }, "currentcolor by default");
    assert_eq!(s.box_shadow[2].color, Color { r: 0, g: 255, b: 0, a: 255 });
    for bad in ["1px", "1px 2px -3px", "1px 2px 3px 4px 5px", "1px 2px wobble", "red blue 1px 2px"] {
        assert!(style_of_inline(&format!("box-shadow: 9px 9px; box-shadow: {bad}")).box_shadow[0].offset_x == 9.0, "{bad} ignored");
    }
    assert!(style_of_inline("box-shadow: 1px 1px; box-shadow: none").box_shadow.is_empty());
    assert!(style_of_inline(&format!("box-shadow: {}", vec!["1px 1px"; 17].join(","))).box_shadow.is_empty(), "too many shadows");
    assert_eq!(style_of_inline("box-shadow: 0 0 1e9px").box_shadow[0].blur, crate::MAX_SHADOW_EXTENT);
}

#[test]
fn before_and_after_get_their_own_styles_and_content() {
    let sheet = css_parser::parse_stylesheet(
        "p { color: #0000ff } p::before { content: '[' attr(id) ']'; font-size: 2em } p:after { content: \"!\"; display: block } \
         p.none::before { content: none } .empty::after { content: '' }",
    );
    let mut p = Element::new("p");
    p.attributes.push(("id".into(), "x".into()));
    let cache = resolve_style_tree_parallel(&p, &sheet, DEFAULT_VIEWPORT_WIDTH);
    let before = &cache[&crate::pseudo_id(p.node_id, crate::PseudoElement::Before)];
    assert_eq!(before.content.as_deref(), Some("[x]"));
    assert_eq!(before.font_size, 32.0);
    assert_eq!(before.color, Color { r: 0, g: 0, b: 255, a: 255 }, "inherits from its element");
    assert_eq!(before.display, crate::Display::Inline, "inline by default");
    let after = &cache[&crate::pseudo_id(p.node_id, crate::PseudoElement::After)];
    assert_eq!((after.content.as_deref(), after.display), (Some("!"), crate::Display::Block), "the legacy :after form");
    assert!(cache[&p.node_id].content.is_none(), "the element's own style is untouched");

    let mut q = Element::new("p");
    q.attributes.push(("class".into(), "none".into()));
    let cache = resolve_style_tree_parallel(&q, &sheet, DEFAULT_VIEWPORT_WIDTH);
    assert!(!cache.contains_key(&crate::pseudo_id(q.node_id, crate::PseudoElement::Before)), "content: none generates nothing");
    let mut d = Element::new("div");
    d.attributes.push(("class".into(), "empty".into()));
    let cache = resolve_style_tree_parallel(&d, &sheet, DEFAULT_VIEWPORT_WIDTH);
    assert_eq!(cache[&crate::pseudo_id(d.node_id, crate::PseudoElement::After)].content.as_deref(), Some(""));
}

#[test]
fn style_stays_small() {
    // Every element's `Style` is built, moved and cached on each restyle:
    // rarely-set property groups live behind a `Box` (Milestone 33 grew it
    // from 328 to 848 bytes and made the cascade ~40% slower; see the M33
    // milestone file).
    let size = std::mem::size_of::<Style>();
    eprintln!("Style is {size} bytes");
    assert!(size <= 512, "Style grew to {size} bytes; box a rarely-set group");
}


#[test]
fn the_dir_attribute_sets_direction_and_it_inherits() {
    let sheet = css_parser::parse_stylesheet("p { color: red }");
    let mut parent = Element::new("div");
    parent.set_attr("dir", "RTL");
    let ps = resolve_style(&parent, &[], None, &sheet);
    assert!(ps.text.rtl);
    let child = Element::new("p");
    assert!(resolve_style(&child, &[&parent], Some(&ps), &sheet).text.rtl, "inherited");
    let mut ltr = Element::new("span");
    ltr.set_attr("dir", "ltr");
    assert!(!resolve_style(&ltr, &[&parent], Some(&ps), &sheet).text.rtl);
    let author = css_parser::parse_stylesheet("div { direction: ltr }");
    assert!(!resolve_style(&parent, &[], None, &author).text.rtl, "author CSS wins over the attribute");
}
