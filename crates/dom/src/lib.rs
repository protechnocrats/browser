//! Minimal DOM node tree.
//!
//! Milestone 1 scope only: elements with attributes and children, plus
//! text nodes. No trust in this crate's *input* — that trust boundary is
//! `html_parser`, which is where untrusted bytes become this structure.
//! This crate just defines the shape.

use std::sync::atomic::{AtomicU64, Ordering};

mod document;
mod element_ref;
pub use document::{Document, ElementMut, Fragment, MutationKind, MutationRecord, Mutations, NodeIdHasher, NodeIdMap, NodeIdSet, NodePath, MAX_MUTATION_RECORDS};
pub use element_ref::{Attributes, ChildRef, Children, ElementRef};
pub use names::{atoms, Atom, AtomTable, Name, STATIC as WELL_KNOWN_NAMES};

/// An element's identity (Milestone 18's `node_id`): unique per process,
/// stable for the element's whole life, and — since Milestone 31 — the ONE
/// key every cache, hit-test path and event dispatch uses. (It replaced
/// `element_ptr_key`, the element's memory address, which was only valid
/// within one borrow of one tree: a key that outlived a navigation could
/// name whatever element the allocator later put at the same address.)
pub type NodeId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// Milestone 18: a process-global counter handing out a fresh, unique
/// `node_id` to every `Element` ever constructed (via `Element::new` —
/// see that fn). This is what lets `js::JsEnvironment` give
/// `document.createElement`'s result a stable handle that survives
/// across separate JS calls, unlike `element_ptr_key` (only valid
/// within one borrow of the tree — see that fn's doc comment). Starts
/// at 1 so `0` stays available as an obviously-invalid sentinel if a
/// caller ever needs one.
static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);

/// Milestone 33: element ids stay below this. The bits above it are
/// reserved for the ids of generated content (`style::pseudo_id`), so an
/// element and a pseudo-element can never share an id. (2^61 elements is
/// unreachable; the allocator still checks.)
pub const MAX_ELEMENT_NODE_ID: NodeId = (1 << 61) - 1;

#[derive(Debug, Clone)]
pub struct Element {
    /// Milestone 40b: names are [`Name`]s — a well-known name is an atom
    /// (no allocation), any other carries its text until the element
    /// enters a document, which interns it (ADR 021 §5).
    pub tag: Name,
    pub attributes: Vec<(Name, String)>,
    pub children: Vec<Node>,
    /// Milestone 18: a stable identity that survives across separate
    /// calls (unlike `element_ptr_key`), assigned once at construction
    /// via `NEXT_NODE_ID`. Deliberately EXCLUDED from `PartialEq` (see
    /// the manual `impl` below) — two independently-constructed,
    /// content-identical elements should still compare equal, and
    /// every existing tree-equality test in this workspace already
    /// relies on that. `Clone` duplicates `node_id` (two `Element`
    /// values sharing one id) — fine as long as clones don't end up
    /// coexisting as separate live tree entries needing distinct
    /// identity, which isn't a pattern this codebase uses today.
    pub node_id: NodeId,
}

/// Manual, not derived: intentionally excludes `node_id` — see that
/// field's doc comment. Otherwise identical to what `#[derive(PartialEq)]`
/// would generate.
impl PartialEq for Element {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.attributes == other.attributes && self.children == other.children
    }
}

/// Iterative, not recursive: the compiler-derived (default) drop glue
/// for a `Vec<Node>` tree recurses once per DOM level with NO depth
/// bound — a sufficiently deep tree overflows the native stack and
/// ABORTS THE WHOLE PROCESS (not a catchable panic) just by being
/// dropped, independent of any bug in code that reads the tree
/// afterward. `html_parser::MAX_NESTING_DEPTH` bounds HTML-PARSED
/// trees, but a tree built directly (e.g. a JS `appendChild` loop, or
/// any other code that constructs an `Element` tree without going
/// through the parser) isn't bounded there — this makes dropping ANY
/// `Element` tree safe regardless of how deep it got or how it was
/// built, by flattening the subtree into a heap-allocated worklist
/// first so no single `drop` call ever recurses more than one level.
impl Drop for Element {
    fn drop(&mut self) {
        let mut worklist: Vec<Node> = std::mem::take(&mut self.children);
        while let Some(node) = worklist.pop() {
            if let Node::Element(mut el) = node {
                worklist.append(&mut el.children);
                // `el` is dropped here with `children` already empty
                // (just moved out above), so ITS `Drop::drop` (this
                // same function, called again by the compiler as `el`
                // goes out of scope) does no further recursive work.
            }
        }
    }
}

/// A fresh, process-unique node id (elements, and since Milestone 39 text
/// nodes in a document).
pub(crate) fn next_node_id() -> NodeId {
    let id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed);
    assert!(id <= MAX_ELEMENT_NODE_ID, "node ids exhausted");
    id
}

/// `element.style.property = value` as plain string rewriting of the
/// inline `style` attribute (see [`Element::set_inline_style_property`]).
pub(crate) fn rewrite_inline_style(existing: Option<&str>, property: &str, value: &str) -> String {
    let mut declarations: Vec<&str> = existing
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter(|d| !d.split_once(':').map(|(k, _)| k.trim() == property).unwrap_or(false))
        .collect();
    let replacement = format!("{property}:{value}");
    declarations.push(&replacement);
    declarations.join(";")
}

impl Element {
    pub fn new(tag: impl Into<Name>) -> Self {
        Element { tag: tag.into(), attributes: Vec::new(), children: Vec::new(), node_id: next_node_id() }
    }

    /// Returns the value of an attribute by name, if present.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The attribute named by the well-known atom `name`.
    pub fn attr_atom(&self, name: Atom) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k.is(name)).map(|(_, v)| v.as_str())
    }

    /// Sets an attribute's value, replacing any existing one with the
    /// same name (or appending a new one). Milestone 16: this is what
    /// `js`'s native text-input editing writes a text input's current
    /// value to (the `value` attribute doubles as "current content" —
    /// see that crate's doc comment for the real-DOM distinction this
    /// simplifies away).
    pub fn set_attr(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(existing) = self.attributes.iter_mut().find(|(k, _)| k == name) {
            existing.1 = value;
        } else {
            self.attributes.push((Name::new(name), value));
        }
    }

    /// Milestone 18: removes an attribute entirely (as opposed to
    /// `set_attr`, which always leaves one behind). A no-op if `name`
    /// isn't present — matching real `Element.removeAttribute`'s
    /// tolerance of removing something that isn't there.
    pub fn remove_attr(&mut self, name: &str) {
        self.attributes.retain(|(k, _)| k != name);
    }

    /// Milestone 18: the `node_id` analogue of `find_by_id`/
    /// `find_by_id_mut` — identical recursive shape, self-then-children.
    pub fn find_by_node_id(&self, node_id: NodeId) -> Option<&Element> {
        if self.node_id == node_id {
            return Some(self);
        }
        self.children.iter().find_map(|c| match c {
            Node::Element(el) => el.find_by_node_id(node_id),
            Node::Text(_) => None,
        })
    }

    pub fn find_by_node_id_mut(&mut self, node_id: NodeId) -> Option<&mut Element> {
        if self.node_id == node_id {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| match c {
            Node::Element(el) => el.find_by_node_id_mut(node_id),
            Node::Text(_) => None,
        })
    }

    /// Milestone 18: finds the descendant with `node_id` anywhere in
    /// this element's subtree, REMOVES it from wherever it currently
    /// sits, and returns it by value — the operation `appendChild`
    /// needs to move a still-detached element (possibly itself nested
    /// inside another still-detached element being built up before
    /// attachment) into a new parent, without leaving a dangling copy
    /// behind. Does not check `self` — only descendants (a node isn't
    /// its own descendant); callers that also need to check top-level
    /// entries do so themselves (see `js::take_detached`).
    pub fn extract_descendant_by_node_id(&mut self, node_id: NodeId) -> Option<Element> {
        if let Some(pos) = self.children.iter().position(|c| matches!(c, Node::Element(e) if e.node_id == node_id)) {
            if let Node::Element(el) = self.children.remove(pos) {
                return Some(el);
            }
        }
        for child in self.children.iter_mut() {
            if let Node::Element(el) = child {
                if let Some(found) = el.extract_descendant_by_node_id(node_id) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Milestone 11/13: depth-first search by `id` attribute — the
    /// mechanism `document.getElementById` is built on. No index/
    /// cache — a plain walk, re-run on every JS access (ids are
    /// stable strings, so this is always correct, just not O(1); a
    /// real DOM would index this, deferred as a Phase 2 performance
    /// concern like everything else in that bucket).
    pub fn find_by_id(&self, id: &str) -> Option<&Element> {
        if self.attr("id") == Some(id) {
            return Some(self);
        }
        self.children.iter().find_map(|c| match c {
            Node::Element(el) => el.find_by_id(id),
            Node::Text(_) => None,
        })
    }

    pub fn find_by_id_mut(&mut self, id: &str) -> Option<&mut Element> {
        if self.attr("id") == Some(id) {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| match c {
            Node::Element(el) => el.find_by_id_mut(id),
            Node::Text(_) => None,
        })
    }

    /// Milestone 11: `document.querySelector` — deliberately minimal,
    /// matching only a single compound selector (`tag`, `.class`,
    /// `#id`, or `tag.class` combinations with no combinators), the
    /// first match in document order. Real selector matching (with
    /// descendant/child combinators) already exists in the `style`
    /// crate for cascade purposes — this is intentionally NOT that;
    /// duplicating `style`'s selector engine here would mean `dom`
    /// depending on `css_parser`/`style`, which it deliberately
    /// doesn't (see architecture doc — `dom` stays a minimal, mostly
    /// dependency-free tree type). A documented scope reduction.
    pub fn query_selector(&self, selector: &str) -> Option<&Element> {
        let selector = selector.trim();
        if self.matches_simple_selector(selector) {
            return Some(self);
        }
        self.children.iter().find_map(|c| match c {
            Node::Element(el) => el.query_selector(selector),
            Node::Text(_) => None,
        })
    }

    fn matches_simple_selector(&self, selector: &str) -> bool {
        if let Some(id) = selector.strip_prefix('#') {
            return self.attr("id") == Some(id);
        }
        if let Some(class) = selector.strip_prefix('.') {
            return self.attr("class").map(|c| c.split_whitespace().any(|w| w == class)).unwrap_or(false);
        }
        // "tag" or "tag.class" or "tag#id" — split off the type
        // selector from whatever follows.
        let split_at = selector.find(['.', '#']).unwrap_or(selector.len());
        let (tag_part, rest) = selector.split_at(split_at);
        if !tag_part.is_empty() && self.tag != tag_part {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        self.matches_simple_selector(rest)
    }

    /// Concatenation of every descendant text node's content — what
    /// `element.textContent` reads in real DOM.
    pub fn text_content(&self) -> String {
        ElementRef::from(self).text_content()
    }

    /// Replaces ALL children with a single text node — what
    /// `element.textContent = "..."` does in real DOM (discards any
    /// existing element children, matching spec behavior).
    pub fn set_text_content(&mut self, text: &str) {
        self.children = vec![Node::Text(text.to_string())];
    }

    /// See [`ElementRef::is_text_editable`] — the one definition.
    pub fn is_text_editable(&self) -> bool {
        ElementRef::from(self).is_text_editable()
    }

    /// See [`ElementRef::editable_value`].
    pub fn editable_value(&self) -> String {
        ElementRef::from(self).editable_value()
    }

    /// `cloneNode(deep)`: a copy with fresh ids (a clone is a new node);
    /// `deep: false` keeps the attributes and drops the children.
    pub fn clone_node(&self, deep: bool) -> Element {
        let mut copy = if deep { self.clone() } else { Element { tag: self.tag.clone(), attributes: self.attributes.clone(), children: Vec::new(), node_id: 0 } };
        document::renumber(&mut copy);
        copy
    }

    pub fn append_child(&mut self, child: Element) {
        self.children.push(Node::Element(child));
    }

    /// Milestone 11: `element.style.property = value` — real browsers
    /// implement this as sugar for rewriting the inline `style`
    /// attribute, which the cascade then picks up on the next layout
    /// pass same as if the author had written it by hand. No parsing
    /// dependency needed (`dom` stays free of `css_parser`) — this is
    /// plain string rewriting: replace an existing `property: ...`
    /// declaration if present, else append one.
    /// Performance note: this used to allocate a fresh `String` per
    /// SURVIVING declaration (`.map(str::to_string)`) even though every
    /// one of them is discarded again a few lines later once
    /// `.join(";")` builds the final owned string — real, unconditional
    /// waste on every single property set (and this is the ONLY way
    /// `element.style.x = ...` from JS reaches the DOM, so any
    /// script animating a style property every frame pays this). Now
    /// keeps the surviving declarations as `&str` slices borrowed from
    /// `existing` and only allocates once, for the final joined string
    /// (plus one allocation for the new declaration's own text) —
    /// same behavior (matching declarations removed, the new one
    /// appended at the end), just without the redundant per-entry
    /// copies.
    pub fn set_inline_style_property(&mut self, property: &str, value: &str) {
        let rewritten = rewrite_inline_style(self.attr("style"), property, value);
        self.set_attr("style", rewritten);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security regression: dropping a VERY deep `Element` tree must
    /// not overflow the native stack. Before `Drop for Element` was
    /// made iterative, this exact test (building then dropping a
    /// 200,000-level-deep tree) crashed the process with a stack
    /// overflow — not from any traversal logic, just from the tree
    /// going out of scope. A tree this deep is reachable from a page
    /// via a JS `appendChild` loop (nothing in this crate bounds JS
    /// -driven tree depth), independent of `html_parser`'s own
    /// `MAX_NESTING_DEPTH` cap on HTML-parsed trees.
    #[test]
    fn dropping_an_extremely_deep_tree_does_not_overflow_the_stack() {
        let depth = 200_000;
        let mut node = Element::new("div");
        for _ in 0..depth {
            let mut parent = Element::new("div");
            parent.children.push(Node::Element(node));
            node = parent;
        }
        drop(node); // the real assertion is just that this returns instead of aborting
    }

    #[test]
    fn attr_lookup() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:10px".into()));
        assert_eq!(el.attr("style"), Some("width:10px"));
        assert_eq!(el.attr("missing"), None);
    }

    #[test]
    fn find_by_id_locates_nested_element() {
        let mut root = Element::new("div");
        let mut child = Element::new("span");
        child.attributes.push(("id".into(), "target".into()));
        root.children.push(Node::Element(child));

        assert_eq!(root.find_by_id("target").map(|e| e.tag.as_str()), Some("span"));
        assert!(root.find_by_id("missing").is_none());
    }

    #[test]
    fn find_by_id_mut_allows_in_place_mutation() {
        let mut root = Element::new("div");
        let mut child = Element::new("span");
        child.attributes.push(("id".into(), "target".into()));
        root.children.push(Node::Element(child));

        root.find_by_id_mut("target").unwrap().set_text_content("hello");
        assert_eq!(root.find_by_id("target").unwrap().text_content(), "hello");
    }

    #[test]
    fn query_selector_matches_tag_class_id() {
        let mut root = Element::new("div");
        let mut p = Element::new("p");
        p.attributes.push(("class".into(), "highlight bold".into()));
        p.attributes.push(("id".into(), "hero".into()));
        root.children.push(Node::Element(p));

        assert!(root.query_selector("p").is_some());
        assert!(root.query_selector(".highlight").is_some());
        assert!(root.query_selector("#hero").is_some());
        assert!(root.query_selector("p.highlight").is_some());
        assert!(root.query_selector("span").is_none());
        assert!(root.query_selector(".nope").is_none());
    }

    #[test]
    fn text_content_concatenates_nested_text_nodes() {
        let mut root = Element::new("div");
        root.children.push(Node::Text("Hello ".to_string()));
        let mut span = Element::new("span");
        span.children.push(Node::Text("world".to_string()));
        root.children.push(Node::Element(span));
        assert_eq!(root.text_content(), "Hello world");
    }

    #[test]
    fn set_text_content_replaces_all_children() {
        let mut root = Element::new("div");
        root.children.push(Node::Element(Element::new("span")));
        root.set_text_content("replaced");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0], Node::Text("replaced".to_string()));
    }

    #[test]
    fn append_child_adds_to_the_end() {
        let mut root = Element::new("div");
        root.append_child(Element::new("span"));
        root.append_child(Element::new("p"));
        assert_eq!(root.children.len(), 2);
        assert!(matches!(&root.children[1], Node::Element(el) if el.tag == "p"));
    }

    #[test]
    fn set_inline_style_property_appends_when_absent() {
        let mut el = Element::new("div");
        el.set_inline_style_property("color", "red");
        assert_eq!(el.attr("style"), Some("color:red"));
    }

    #[test]
    fn set_inline_style_property_replaces_existing_declaration() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:10px;color:blue".into()));
        el.set_inline_style_property("color", "red");
        let style = el.attr("style").unwrap();
        assert!(style.contains("color:red"));
        assert!(!style.contains("blue"));
        assert!(style.contains("width:10px"));
    }

    #[test]
    fn each_new_element_gets_a_unique_node_id() {
        let a = Element::new("div");
        let b = Element::new("div");
        assert_ne!(a.node_id, b.node_id);
    }

    #[test]
    fn equality_ignores_node_id() {
        // Same tag/attributes/children, independently constructed —
        // different node_ids, but must still compare equal. This is
        // the regression this project's own equality-based parser
        // tests (an "expected" tree built by hand vs. the real parsed
        // one) all depend on.
        let a = Element::new("div");
        let b = Element::new("div");
        assert_ne!(a.node_id, b.node_id);
        assert_eq!(a, b);
    }

    #[test]
    fn find_by_node_id_locates_nested_element() {
        let mut root = Element::new("div");
        let child = Element::new("span");
        let child_id = child.node_id;
        root.children.push(Node::Element(child));

        assert_eq!(root.find_by_node_id(child_id).map(|e| e.tag.as_str()), Some("span"));
        assert!(root.find_by_node_id(u64::MAX).is_none());
    }

    #[test]
    fn find_by_node_id_mut_allows_in_place_mutation() {
        let mut root = Element::new("div");
        let child = Element::new("span");
        let child_id = child.node_id;
        root.children.push(Node::Element(child));

        root.find_by_node_id_mut(child_id).unwrap().set_text_content("hello");
        assert_eq!(root.find_by_node_id(child_id).unwrap().text_content(), "hello");
    }

    #[test]
    fn extract_descendant_by_node_id_removes_and_returns_it() {
        let mut root = Element::new("div");
        let child = Element::new("span");
        let child_id = child.node_id;
        root.children.push(Node::Element(child));

        let extracted = root.extract_descendant_by_node_id(child_id).expect("should find and extract");
        assert_eq!(extracted.tag, "span");
        assert!(root.children.is_empty(), "the extracted node should no longer be in the tree");
        assert!(root.extract_descendant_by_node_id(child_id).is_none(), "extracting twice finds nothing the second time");
    }

    #[test]
    fn extract_descendant_by_node_id_finds_grandchildren() {
        let mut root = Element::new("div");
        let mut mid = Element::new("div");
        let grandchild = Element::new("span");
        let grandchild_id = grandchild.node_id;
        mid.children.push(Node::Element(grandchild));
        root.children.push(Node::Element(mid));

        let extracted = root.extract_descendant_by_node_id(grandchild_id).expect("should find nested descendant");
        assert_eq!(extracted.tag, "span");
    }

    #[test]
    fn text_editability_is_decided_in_one_place() {
        let with = |tag: &str, attrs: &[(&str, &str)]| {
            let mut el = Element::new(tag);
            for (k, v) in attrs {
                el.set_attr(k, *v);
            }
            el
        };
        assert!(with("input", &[]).is_text_editable());
        assert!(with("input", &[("type", "text")]).is_text_editable());
        assert!(with("input", &[("type", "search")]).is_text_editable());
        assert!(with("textarea", &[]).is_text_editable());
        assert!(with("div", &[("contenteditable", "")]).is_text_editable());
        assert!(with("div", &[("contenteditable", "true")]).is_text_editable());
        for t in ["submit", "checkbox", "radio"] {
            assert!(!with("input", &[("type", t)]).is_text_editable(), "{t}");
        }
        assert!(!with("div", &[("contenteditable", "false")]).is_text_editable());
        assert!(!with("div", &[]).is_text_editable());

        let mut note = with("div", &[("contenteditable", "")]);
        note.set_text_content("hi");
        assert_eq!(note.editable_value(), "hi");
        assert_eq!(with("input", &[("value", "v")]).editable_value(), "v");
    }

    #[test]
    fn remove_attr_removes_a_present_attribute_and_tolerates_a_missing_one() {
        let mut el = Element::new("div");
        el.set_attr("class", "x");
        el.remove_attr("class");
        assert_eq!(el.attr("class"), None);
        el.remove_attr("class"); // must not panic on a second removal
    }
}
