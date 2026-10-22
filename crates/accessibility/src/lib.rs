//! Milestone 24: an accessibility tree — one `AccessibleNode` per
//! `dom::Element`, each with a computed **role** (native HTML
//! semantics, not explicit ARIA authoring — see `Role`'s doc comment),
//! an **accessible name** (a simplified, precedence-ordered version of
//! the real `accname` computation browsers use), and **state**
//! (focused/checked/disabled/required/invalid).
//!
//! This crate sits on the "core" side of the dependency-direction
//! invariant `dom`/`style`/`layout`/`rendering` already hold — no
//! `wgpu`/`winit`, checked by
//! `scripts/check-dependency-direction.sh` alongside them. It produces
//! the DATA a real platform accessibility integration (Linux AT-SPI,
//! Windows UIA, macOS NSAccessibility, or a cross-platform wrapper
//! like the `accesskit` crate) would consume — wiring this tree up to
//! an actual screen reader is deliberately NOT done here; see
//! `milestones/milestone-24-accessibility-tree.md`'s "Out of scope"
//! section for why that's real, separate, platform-specific work.

#![forbid(unsafe_code)]

use dom::{ChildRef, ElementRef};

/// The accessible role of an element — HTML-AAM-inspired native
/// semantics only (what role a plain, unstyled, no-`role`-attribute
/// element implies). Explicit `role="..."` authoring overrides aren't
/// read yet — a real, documented scope cut (see the milestone doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Link,
    Button,
    TextBox,
    CheckBox,
    RadioButton,
    /// `<h1>`–`<h6>`, with the real 1-6 level (not just "it's a
    /// heading") — screen readers use the level for jump-by-heading
    /// navigation, so this is real information, not decoration.
    Heading(u8),
    Image,
    List,
    ListItem,
    Form,
    Paragraph,
    /// Every other element — real browsers assign a plain `<div>` no
    /// special role either; this is that same fallback, not "unknown"
    /// or an error case.
    Generic,
}

/// Interactive state, computed against the live DOM/focus — the same
/// state Milestones 16/19/20 already track for rendering purposes,
/// re-read here for accessibility purposes rather than duplicating
/// where it's tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessibleState {
    pub focused: bool,
    /// `Some` only for checkbox/radio roles — `None` isn't "false", it
    /// means "checked-ness doesn't apply to this element at all"
    /// (matches real ARIA's `aria-checked` being undefined for a
    /// non-checkable element, rather than defaulting to false).
    pub checked: Option<bool>,
    pub disabled: bool,
    pub required: bool,
    /// A text box whose value breaks its constraints (`required`,
    /// `pattern`) — `dom`'s rule, the same one `:invalid` and form
    /// submission use (ADR 021 §6; it was a narrower copy, M24).
    pub invalid: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccessibleNode {
    pub role: Role,
    pub name: Option<String>,
    pub state: AccessibleState,
    pub children: Vec<AccessibleNode>,
}

/// Builds the accessible role for `element` from native HTML
/// semantics — `None` for `Generic` would be equally valid, but a
/// concrete `Role::Generic` variant (rather than `Option<Role>`) keeps
/// every node in the tree structurally uniform, matching how real
/// accessibility trees always assign SOME role.
fn role_for(element: ElementRef<'_>) -> Role {
    match element.tag() {
        "a" if element.attr_atom(dom::atoms::HREF).is_some() => Role::Link,
        "button" => Role::Button,
        "input" => match element.attr_atom(dom::atoms::TYPE) {
            Some("submit") | Some("button") => Role::Button,
            Some("checkbox") => Role::CheckBox,
            Some("radio") => Role::RadioButton,
            _ => Role::TextBox, // text, email, search, unset, ... — the common default
        },
        "textarea" => Role::TextBox,
        "h1" => Role::Heading(1),
        "h2" => Role::Heading(2),
        "h3" => Role::Heading(3),
        "h4" => Role::Heading(4),
        "h5" => Role::Heading(5),
        "h6" => Role::Heading(6),
        "img" => Role::Image,
        "ul" | "ol" => Role::List,
        "li" => Role::ListItem,
        "form" => Role::Form,
        "p" => Role::Paragraph,
        // `dom`'s editability rule (ADR 021 §6): here, `contenteditable`.
        _ if element.is_text_editable() => Role::TextBox,
        _ => Role::Generic,
    }
}

/// Whether `role` is a "name from content" role — real browsers
/// compute these elements' accessible name from their own rendered
/// text when nothing more specific (`aria-label`, a `<label>`, ...)
/// overrides it. Roles like `TextBox`/`CheckBox` deliberately are NOT
/// in this list — an `<input>`'s own text content is never its name
/// (it has none; its name comes from a `<label>` or `aria-label`).
fn name_from_content(role: Role) -> bool {
    matches!(role, Role::Link | Role::Button | Role::Heading(_) | Role::Paragraph | Role::ListItem)
}

/// Computes `element`'s accessible name, in real precedence order:
/// `aria-label` > `aria-labelledby` (space-separated ids, each
/// resolved to ITS OWN text content and joined with a space) > an
/// associated `<label for=id>` found anywhere in the document (via
/// `labels_by_for`, built once per tree by `collect_labels`) > `alt`
/// (images) > the element's own text content, for
/// `name_from_content` roles only > no name.
fn accessible_name(element: ElementRef<'_>, role: Role, labels_by_for: &std::collections::HashMap<String, String>, own_text: &str) -> Option<String> {
    if let Some(label) = element.attr_atom(dom::atoms::ARIA_LABEL) {
        let normalized = normalize_whitespace(label);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    if let Some(ids) = element.attr_atom(dom::atoms::ARIA_LABELLEDBY) {
        let joined: Vec<String> = ids
            .split_whitespace()
            .filter_map(|id| labels_by_for.get(&format!("\0text-content:{id}")).cloned())
            .collect();
        if !joined.is_empty() {
            return Some(joined.join(" "));
        }
    }
    if let Some(id) = element.attr_atom(dom::atoms::ID) {
        if let Some(label_text) = labels_by_for.get(id) {
            return Some(label_text.clone());
        }
    }
    if let Some(alt) = element.attr_atom(dom::atoms::ALT) {
        let normalized = normalize_whitespace(alt);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    if name_from_content(role) {
        // `own_text` is precomputed once, bottom-up, by
        // `compute_text_contents` — see that function's doc comment for
        // why this must NOT be a fresh `element.text_content()` call
        // here (an O(n^2) trap on adversarial-but-plausible markup).
        let normalized = normalize_whitespace(own_text);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    None
}

/// Collapses any run of whitespace (including newlines/indentation
/// from how the HTML source happens to be formatted) to a single
/// space, and trims the ends — matching how real accessible-name
/// computation flattens text into one plain string rather than
/// preserving source formatting (a screen reader shouldn't read out
/// "a real [pause for the source's line break and indentation]
/// external link" for text that's visually just "a real external
/// link" once laid out and wrapped).
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn state_for(element: ElementRef<'_>, role: Role, focused_id: Option<&str>) -> AccessibleState {
    let focused = matches!((element.attr_atom(dom::atoms::ID), focused_id), (Some(id), Some(focused)) if id == focused);
    let checked = matches!(role, Role::CheckBox | Role::RadioButton).then(|| element.attr_atom(dom::atoms::CHECKED).is_some());
    let disabled = element.attr_atom(dom::atoms::DISABLED).is_some();
    let required = element.attr_atom(dom::atoms::REQUIRED).is_some();
    let invalid = matches!(role, Role::TextBox) && !element.is_valid();
    AccessibleState { focused, checked, disabled, required, invalid }
}

/// Walks the whole tree once collecting two lookups `accessible_name`
/// needs: `<label for="x">`'s own text content, keyed by `x` (the
/// native label-association mechanism), and EVERY element's text
/// content keyed under a `\0text-content:` sentinel prefix by its
/// `id` (what `aria-labelledby` resolves against — a different
/// association than `for`, sharing one map rather than two since
/// nothing else needs a second pass over the tree). The sentinel
/// prefix is a real, deliberate hack to keep this a single map without
/// a real risk of an actual `id` colliding with it (`\0` can't appear
/// in a parsed HTML `id` attribute).
/// Caps recursion depth across every tree walk in this file. Two real
/// problems without it: (1) `element.text_content()` (a full recursive
/// subtree walk) used to be called once per labeled/nameable element in
/// `collect_labels`/`accessible_name` — for N elements nested N deep,
/// each carrying an `id` or a name-from-content role, that's O(n^2)
/// total (see `compute_text_contents` for the O(n) replacement); (2)
/// even at O(n), a sufficiently deep tree still recurses once per DOM
/// level with no bound of its own and can overflow the native stack.
/// `html_parser::MAX_NESTING_DEPTH` already caps HTML-parsed trees at
/// 512, but a tree built/mutated via JS `appendChild` isn't bounded
/// there, so this crate enforces its own limit independent of how the
/// tree was constructed. Past the cap, a subtree is simply treated as
/// empty/leaf rather than continuing to recurse — degrading gracefully
/// rather than crashing, matching this project's stated discipline for
/// untrusted input elsewhere (see `html_parser`'s module doc comment).
const MAX_TREE_DEPTH: usize = 2048;

/// Computes every element's own text content in a SINGLE bottom-up
/// pass: each element's text is its children's texts concatenated, and
/// a child's text is only ever computed once (via this function's own
/// return value), never re-walked from an ancestor — the fix for the
/// O(n^2) blowup `element.text_content()` caused when called per
/// labeled element. Populates `out`, keyed by `dom::element_ptr_key`,
/// with every element's own (un-normalized) text content, and returns
/// `element`'s own text content directly so a caller several levels up
/// doesn't need a second lookup.
fn compute_text_contents(element: ElementRef<'_>, out: &mut std::collections::HashMap<dom::NodeId, String>, depth: usize) -> String {
    if depth >= MAX_TREE_DEPTH {
        diagnostics::guard(diagnostics::Guard::AccessibilityDepth, "tree deeper than the accessibility walk; subtree skipped");
        return String::new();
    }
    let mut text = String::new();
    for child in element.children() {
        match child {
            ChildRef::Text(t) => text.push_str(t),
            ChildRef::Element(el) => text.push_str(&compute_text_contents(el, out, depth + 1)),
        }
    }
    out.insert(element.node_id(), text.clone());
    text
}

fn collect_labels(
    element: ElementRef<'_>,
    out: &mut std::collections::HashMap<String, String>,
    text_contents: &std::collections::HashMap<dom::NodeId, String>,
    depth: usize,
) {
    if depth >= MAX_TREE_DEPTH {
        return;
    }
    let own_text = text_contents.get(&element.node_id()).map(String::as_str).unwrap_or("");
    if element.is(dom::atoms::LABEL) {
        if let Some(for_id) = element.attr_atom(dom::atoms::FOR) {
            let normalized = normalize_whitespace(own_text);
            if !normalized.is_empty() {
                out.insert(for_id.to_string(), normalized);
            }
        }
    }
    if let Some(id) = element.attr_atom(dom::atoms::ID) {
        let normalized = normalize_whitespace(own_text);
        if !normalized.is_empty() {
            out.insert(format!("\0text-content:{id}"), normalized);
        }
    }
    for child in element.children() {
        if let ChildRef::Element(el) = child {
            collect_labels(el, out, text_contents, depth + 1);
        }
    }
}

/// Builds the full accessibility tree rooted at `root`. `focused_id`
/// is the currently focused element's `id` attribute (the same
/// identity `js::JsEnvironment::focused_element_id` already tracks —
/// this crate doesn't depend on `js`, so the caller passes it in,
/// matching the `app`/`js` boundary every other cross-cutting piece of
/// state in this project already crosses this way).
pub fn build_tree<'a>(root: impl Into<ElementRef<'a>>, focused_id: Option<&str>) -> AccessibleNode {
    let root = root.into();
    let mut text_contents = std::collections::HashMap::new();
    compute_text_contents(root, &mut text_contents, 0);
    let mut labels_by_for = std::collections::HashMap::new();
    collect_labels(root, &mut labels_by_for, &text_contents, 0);
    build_node(root, focused_id, &labels_by_for, &text_contents, 0)
}

fn build_node(
    element: ElementRef<'_>,
    focused_id: Option<&str>,
    labels_by_for: &std::collections::HashMap<String, String>,
    text_contents: &std::collections::HashMap<dom::NodeId, String>,
    depth: usize,
) -> AccessibleNode {
    let role = role_for(element);
    let own_text = text_contents.get(&element.node_id()).map(String::as_str).unwrap_or("");
    let name = accessible_name(element, role, labels_by_for, own_text);
    let state = state_for(element, role, focused_id);
    let children = if depth >= MAX_TREE_DEPTH {
        Vec::new()
    } else {
        element.element_children().map(|el| build_node(el, focused_id, labels_by_for, text_contents, depth + 1)).collect()
    };
    AccessibleNode { role, name, state, children }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::{Element, Node};

    fn el(tag: &str) -> Element {
        Element::new(tag)
    }

    fn with_attr(mut e: Element, name: &str, value: &str) -> Element {
        e.attributes.push((name.into(), value.to_string()));
        e
    }

    fn with_text(mut e: Element, text: &str) -> Element {
        e.children.push(Node::Text(text.to_string()));
        e
    }

    fn with_child(mut e: Element, child: Element) -> Element {
        e.children.push(Node::Element(child));
        e
    }

    // ---- Role mapping ----

    #[test]
    fn link_role_requires_a_real_href() {
        let with_href = with_attr(el("a"), "href", "/x");
        assert_eq!(role_for(dom::ElementRef::from(&with_href)), Role::Link);
        let without_href = el("a");
        assert_eq!(role_for(dom::ElementRef::from(&without_href)), Role::Generic, "an <a> with no href is not a real link");
    }

    #[test]
    fn button_role_covers_button_tag_and_input_submit_button() {
        assert_eq!(role_for(dom::ElementRef::from(&el("button"))), Role::Button);
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("input"), "type", "submit"))), Role::Button);
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("input"), "type", "button"))), Role::Button);
    }

    #[test]
    fn checkbox_and_radio_roles() {
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("input"), "type", "checkbox"))), Role::CheckBox);
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("input"), "type", "radio"))), Role::RadioButton);
    }

    #[test]
    fn plain_text_input_and_textarea_and_contenteditable_are_textboxes() {
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("input"), "type", "text"))), Role::TextBox);
        assert_eq!(role_for(dom::ElementRef::from(&el("input"))), Role::TextBox, "no type attribute defaults to text");
        assert_eq!(role_for(dom::ElementRef::from(&el("textarea"))), Role::TextBox);
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("div"), "contenteditable", "true"))), Role::TextBox);
        assert_eq!(role_for(dom::ElementRef::from(&with_attr(el("div"), "contenteditable", "false"))), Role::Generic);
    }

    #[test]
    fn heading_levels_are_real_not_just_tagged_as_headings() {
        for (tag, level) in [("h1", 1), ("h2", 2), ("h3", 3), ("h4", 4), ("h5", 5), ("h6", 6)] {
            assert_eq!(role_for(dom::ElementRef::from(&el(tag))), Role::Heading(level));
        }
    }

    #[test]
    fn list_list_item_form_paragraph_and_image_roles() {
        assert_eq!(role_for(dom::ElementRef::from(&el("ul"))), Role::List);
        assert_eq!(role_for(dom::ElementRef::from(&el("ol"))), Role::List);
        assert_eq!(role_for(dom::ElementRef::from(&el("li"))), Role::ListItem);
        assert_eq!(role_for(dom::ElementRef::from(&el("form"))), Role::Form);
        assert_eq!(role_for(dom::ElementRef::from(&el("p"))), Role::Paragraph);
        assert_eq!(role_for(dom::ElementRef::from(&el("img"))), Role::Image);
    }

    #[test]
    fn an_unstyled_div_gets_the_generic_fallback_role() {
        assert_eq!(role_for(dom::ElementRef::from(&el("div"))), Role::Generic);
    }

    // ---- Accessible name precedence ----

    #[test]
    fn aria_label_wins_over_everything_else() {
        let button = with_text(with_attr(el("button"), "aria-label", "Close dialog"), "X");
        let node = build_tree(&button, None);
        assert_eq!(node.name, Some("Close dialog".to_string()));
    }

    #[test]
    fn aria_labelledby_resolves_a_referenced_elements_text_content() {
        let heading = with_text(with_attr(el("h2"), "id", "section-title"), "Billing details");
        let button = with_attr(el("button"), "aria-labelledby", "section-title");
        let root = with_child(with_child(el("div"), heading), button);
        let node = build_tree(&root, None);
        assert_eq!(node.children[1].name, Some("Billing details".to_string()));
    }

    #[test]
    fn a_label_for_beats_text_content_and_alt() {
        let label = with_text(with_attr(el("label"), "for", "email"), "Email address");
        let input = with_attr(with_attr(el("input"), "id", "email"), "type", "text");
        let root = with_child(with_child(el("form"), label), input);
        let node = build_tree(&root, None);
        assert_eq!(node.children[1].name, Some("Email address".to_string()));
    }

    #[test]
    fn alt_provides_the_name_for_an_image() {
        let img = with_attr(el("img"), "alt", "Company logo");
        let node = build_tree(&img, None);
        assert_eq!(node.name, Some("Company logo".to_string()));
    }

    #[test]
    fn text_content_names_a_link_or_button_with_nothing_else_set() {
        let link = with_text(with_attr(el("a"), "href", "/about"), "About us");
        let node = build_tree(&link, None);
        assert_eq!(node.name, Some("About us".to_string()));
    }

    #[test]
    fn an_input_never_gets_its_name_from_its_own_text_content() {
        // Inputs are void elements in real HTML (no text children at
        // all), but even if something odd ended up as a child, an
        // input's name must come from a label/aria-label, never from
        // "content" — TextBox isn't a name-from-content role.
        let mut input = with_attr(el("input"), "type", "text");
        input.children.push(Node::Text("stray text".into()));
        let node = build_tree(&input, None);
        assert_eq!(node.name, None);
    }

    #[test]
    fn an_element_with_no_name_source_at_all_has_no_name() {
        let div = el("div");
        let node = build_tree(&div, None);
        assert_eq!(node.name, None);
    }

    #[test]
    fn empty_aria_label_falls_through_to_the_next_precedence_level() {
        let link = with_text(with_attr(el("a"), "aria-label", "  "), "Real text");
        let link = with_attr(link, "href", "/x");
        let node = build_tree(&link, None);
        assert_eq!(node.name, Some("Real text".to_string()), "whitespace-only aria-label must not win");
    }

    #[test]
    fn text_content_name_collapses_source_formatting_whitespace() {
        // Real HTML source commonly wraps/indents text across lines —
        // the accessible name must read as one flat, space-joined
        // string, not preserve the newline/indentation.
        let mut link = with_attr(el("a"), "href", "/x");
        link.children.push(Node::Text("a real\n        external link".to_string()));
        let node = build_tree(&link, None);
        assert_eq!(node.name, Some("a real external link".to_string()));
    }

    // ---- State ----

    #[test]
    fn focused_state_matches_the_focused_element_id() {
        let input = with_attr(el("input"), "id", "search");
        let node = build_tree(&input, Some("search"));
        assert!(node.state.focused);
        let node_unfocused = build_tree(&input, Some("other"));
        assert!(!node_unfocused.state.focused);
        let node_no_focus = build_tree(&input, None);
        assert!(!node_no_focus.state.focused);
    }

    #[test]
    fn checked_is_some_only_for_checkbox_and_radio_roles() {
        let checked_box = with_attr(with_attr(el("input"), "type", "checkbox"), "checked", "checked");
        assert_eq!(build_tree(&checked_box, None).state.checked, Some(true));

        let unchecked_box = with_attr(el("input"), "type", "checkbox");
        assert_eq!(build_tree(&unchecked_box, None).state.checked, Some(false));

        let text_input = with_attr(el("input"), "type", "text");
        assert_eq!(build_tree(&text_input, None).state.checked, None, "checked doesn't apply to a text box at all");
    }

    #[test]
    fn disabled_and_required_reflect_the_real_attributes() {
        let el1 = with_attr(with_attr(el("input"), "disabled", "disabled"), "required", "required");
        let node = build_tree(&el1, None);
        assert!(node.state.disabled);
        assert!(node.state.required);

        let el2 = el("input");
        let node2 = build_tree(&el2, None);
        assert!(!node2.state.disabled);
        assert!(!node2.state.required);
    }

    #[test]
    fn a_pattern_mismatch_is_invalid_as_style_and_submission_see_it() {
        // Milestone 39 (ADR 021 §6): one rule — it used to ignore `pattern`.
        let mut zip = with_attr(el("input"), "pattern", "[0-9]{5}");
        zip = with_attr(zip, "value", "12ab");
        assert!(build_tree(&zip, None).state.invalid);
        assert!(!dom::ElementRef::from(&zip).is_valid(), "the same answer `:invalid` and submission get");
        let ok = with_attr(with_attr(el("input"), "pattern", "[0-9]{5}"), "value", "12345");
        assert!(!build_tree(&ok, None).state.invalid);
    }

    #[test]
    fn invalid_is_true_only_for_a_required_and_empty_text_box() {
        let empty_required = with_attr(with_attr(el("input"), "type", "text"), "required", "required");
        assert!(build_tree(&empty_required, None).state.invalid);

        let filled_required = with_attr(with_attr(with_attr(el("input"), "type", "text"), "required", "required"), "value", "something");
        assert!(!build_tree(&filled_required, None).state.invalid);

        let not_required = with_attr(el("input"), "type", "text");
        assert!(!build_tree(&not_required, None).state.invalid);
    }

    // ---- Whole-tree structure ----

    #[test]
    fn text_nodes_are_not_represented_as_separate_accessible_nodes() {
        let p = with_text(el("p"), "hello world");
        let node = build_tree(&p, None);
        assert_eq!(node.children.len(), 0, "a text child must not produce a spurious child AccessibleNode");
        assert_eq!(node.name, Some("hello world".to_string()));
    }

    #[test]
    fn nested_structure_is_preserved() {
        let li1 = with_text(el("li"), "first");
        let li2 = with_text(el("li"), "second");
        let list = with_child(with_child(el("ul"), li1), li2);
        let node = build_tree(&list, None);
        assert_eq!(node.role, Role::List);
        assert_eq!(node.children.len(), 2);
        assert_eq!(node.children[0].role, Role::ListItem);
        assert_eq!(node.children[0].name, Some("first".to_string()));
        assert_eq!(node.children[1].name, Some("second".to_string()));
    }

    // ---- Against a real, realistic document (not just synthetic fixtures) ----

    #[test]
    fn builds_a_correct_tree_from_a_realistic_document() {
        let html = r#"
            <html><body>
                <h1 id="headline">Welcome</h1>
                <form id="search-form">
                    <label for="search">Search</label>
                    <input id="search" type="text" required>
                    <button id="search-submit">Search</button>
                </form>
                <input id="agree" type="checkbox" checked>
                <a id="nav-link" href="https://example.com/">a real external link</a>
            </body></html>
        "#;
        let root = html_parser::parse_document(html).unwrap();
        let body = root
            .children
            .iter()
            .find_map(|n| match n {
                Node::Element(el) if el.tag == "body" => Some(el),
                _ => None,
            })
            .expect("parsed document should have a body");

        let tree = build_tree(body, Some("search"));

        fn find_by_id<'a>(node: &'a AccessibleNode, target_id: &str, el: &'a Element) -> Option<(&'a AccessibleNode, &'a Element)> {
            if el.attr_atom(dom::atoms::ID) == Some(target_id) {
                return Some((node, el));
            }
            for (child_node, child_el) in node.children.iter().zip(el.children.iter().filter_map(|c| match c {
                Node::Element(e) => Some(e),
                Node::Text(_) => None,
            })) {
                if let Some(found) = find_by_id(child_node, target_id, child_el) {
                    return Some(found);
                }
            }
            None
        }

        let (headline_node, _) = find_by_id(&tree, "headline", body).expect("headline should exist");
        assert_eq!(headline_node.role, Role::Heading(1));
        assert_eq!(headline_node.name, Some("Welcome".to_string()));

        let (search_node, _) = find_by_id(&tree, "search", body).expect("search input should exist");
        assert_eq!(search_node.role, Role::TextBox);
        assert_eq!(search_node.name, Some("Search".to_string()), "should pick up the associated <label for=search>");
        assert!(search_node.state.focused);
        assert!(search_node.state.required);
        assert!(search_node.state.invalid, "required and empty");

        let (submit_node, _) = find_by_id(&tree, "search-submit", body).expect("submit button should exist");
        assert_eq!(submit_node.role, Role::Button);
        assert_eq!(submit_node.name, Some("Search".to_string()), "button's own text content");

        let (agree_node, _) = find_by_id(&tree, "agree", body).expect("checkbox should exist");
        assert_eq!(agree_node.role, Role::CheckBox);
        assert_eq!(agree_node.state.checked, Some(true));

        let (link_node, _) = find_by_id(&tree, "nav-link", body).expect("link should exist");
        assert_eq!(link_node.role, Role::Link);
        assert_eq!(link_node.name, Some("a real external link".to_string()));
    }

    // ---- Security regressions ----

    /// Without `MAX_TREE_DEPTH`, a tree deeper than the native stack
    /// can handle overflows the process (not a catchable panic) in
    /// `compute_text_contents`/`collect_labels`/`build_node`, all of
    /// which used to recurse one native frame per DOM level with no
    /// bound. `html_parser` caps HTML-PARSED trees at 512, but a tree
    /// built directly (as this test does, bypassing the parser — the
    /// same shape a JS `appendChild` loop could produce) isn't bounded
    /// there, so this crate must enforce its own limit. This must
    /// complete without crashing.
    #[test]
    fn tree_deeper_than_the_cap_does_not_overflow_the_stack() {
        let depth = 100_000;
        let mut leaf = el("div");
        leaf.attributes.push(("id".into(), "innermost".to_string()));
        let mut root = leaf;
        for _ in 0..depth {
            root = with_child(el("div"), root);
        }
        let tree = build_tree(&root, None);
        // Just proving it returned at all (didn't crash) is the point;
        // the tree is truncated at MAX_TREE_DEPTH rather than fully
        // represented.
        let _ = tree;
    }

    /// Security regression: `collect_labels`/`accessible_name` used to
    /// call `element.text_content()` (a full recursive subtree walk)
    /// once per id'd/nameable element — O(n) work times O(n) elements.
    /// This builds a document where every one of N nested elements has
    /// an `id`, the exact adversarial-but-plausible shape that made the
    /// old code quadratic, and asserts `build_tree` still completes
    /// quickly. Depth is kept under `MAX_TREE_DEPTH` so this measures
    /// the O(n) fix specifically, not the separate depth cap above.
    #[test]
    fn build_tree_is_fast_even_when_every_nested_element_has_an_id() {
        let n = 1500;
        let mut node = el("a");
        node.attributes.push(("id".into(), "id-0".to_string()));
        node.attributes.push(("href".into(), "/x".to_string()));
        node.children.push(Node::Text("leaf".to_string()));
        for i in 1..n {
            let mut wrapper = el("a");
            wrapper.attributes.push(("id".into(), format!("id-{i}")));
            wrapper.attributes.push(("href".into(), "/x".to_string()));
            wrapper.children.push(Node::Element(node));
            node = wrapper;
        }
        let start = std::time::Instant::now();
        let _tree = build_tree(&node, None);
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "O(n^2) text-content recomputation would make this take much longer at n={n}");
    }

    // ---- Milestone 27: the a11y depth cap is observable ----

    #[test]
    fn a_tree_past_the_accessibility_cap_reports_the_guard() {
        use diagnostics::{collector::capture, Guard};
        let mut root = el("div");
        for _ in 0..(MAX_TREE_DEPTH + 10) {
            root = with_child(el("div"), root);
        }
        let (_, c) = capture(|| build_tree(&root, None));
        assert!(c.saw_guard(Guard::AccessibilityDepth));

        let (_, c) = capture(|| build_tree(&with_child(el("div"), el("p")), None));
        assert!(c.events.is_empty(), "a shallow tree trips nothing");
    }
}
