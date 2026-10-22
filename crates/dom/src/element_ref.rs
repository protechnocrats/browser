//! Milestone 39 (ADR 021): the one way every other crate reads elements.
//!
//! [`ElementRef`] is a `Copy` handle to an element wherever it lives — an
//! owned [`Element`] (a fragment: generated content, a detached subtree, a
//! test's tree) or a node of a live [`Document`]'s arena. Readers (style,
//! layout, accessibility, the engine, the bindings) take `ElementRef`s and
//! never see how the tree is stored. Every walk here is iterative: a
//! script-built tree can be arbitrarily deep.

use crate::document::{Data, Link, NodeData, Slot};
use crate::{Atom, Document, Element, Name, Node, NodeId};

/// A handle to an element. Cheap to copy; borrows the tree it points into.
#[derive(Clone, Copy)]
pub struct ElementRef<'a> {
    inner: Inner<'a>,
}

#[derive(Clone, Copy)]
enum Inner<'a> {
    Owned(&'a Element),
    /// The node's data is held directly: reads of the tag and attributes —
    /// the hot path of style and layout — do not go back through the arena.
    Arena(&'a Document, &'a NodeData),
}

/// An element's attributes as `(name, value)`, in source order.
pub struct Attributes<'a> {
    /// The document's table, for an arena element's names.
    atoms: Option<&'a names::AtomTable>,
    list: std::slice::Iter<'a, (Name, String)>,
}

impl<'a> Iterator for Attributes<'a> {
    type Item = (&'a str, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        let (k, v) = self.list.next()?;
        let k = match self.atoms {
            Some(atoms) => atoms.resolve_name(k),
            None => k.as_str(),
        };
        Some((k, v.as_str()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.list.size_hint()
    }
}

impl ExactSizeIterator for Attributes<'_> {}

/// One child of an element, as readers see it: an element or a text node.
#[derive(Clone, Copy, Debug)]
pub enum ChildRef<'a> {
    Element(ElementRef<'a>),
    Text(&'a str),
}

impl<'a> ChildRef<'a> {
    /// The child, if it is an element.
    pub fn as_element(self) -> Option<ElementRef<'a>> {
        match self {
            ChildRef::Element(e) => Some(e),
            ChildRef::Text(_) => None,
        }
    }
}

/// An owned node (generated content, a fragment) seen as a child.
impl<'a> From<&'a Node> for ChildRef<'a> {
    fn from(node: &'a Node) -> Self {
        owned_child(node)
    }
}

impl<'a> From<&'a Element> for ElementRef<'a> {
    fn from(element: &'a Element) -> Self {
        ElementRef { inner: Inner::Owned(element) }
    }
}

impl<'a> From<&'a Document> for ElementRef<'a> {
    fn from(document: &'a Document) -> Self {
        document.root()
    }
}

/// Two handles are equal when they name the same node.
impl PartialEq for ElementRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.node_id() == other.node_id()
    }
}

impl std::fmt::Debug for ElementRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElementRef").field("tag", &self.tag()).field("node_id", &self.node_id()).finish()
    }
}

/// The children of an element, in document order.
pub struct Children<'a> {
    inner: ChildIter<'a>,
}

enum ChildIter<'a> {
    Owned(std::slice::Iter<'a, Node>),
    Arena { document: &'a Document, front: Link, back: Link, remaining: usize },
}

fn owned_child(node: &Node) -> ChildRef<'_> {
    match node {
        Node::Element(e) => ChildRef::Element(e.into()),
        Node::Text(t) => ChildRef::Text(t),
    }
}

/// A child already looked up: one arena access per step of a walk.
fn arena_child<'a>(document: &'a Document, node: &'a NodeData) -> ChildRef<'a> {
    match &node.data {
        Data::Element { .. } => ChildRef::Element(ElementRef { inner: Inner::Arena(document, node) }),
        Data::Text(t) => ChildRef::Text(t),
        Data::Free => unreachable!("a free slot is never linked"),
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = ChildRef<'a>;
    fn next(&mut self) -> Option<ChildRef<'a>> {
        match &mut self.inner {
            ChildIter::Owned(iter) => iter.next().map(owned_child),
            ChildIter::Arena { document, front, remaining, .. } => {
                if *remaining == 0 {
                    return None;
                }
                let node = document.node(front.get()?);
                *remaining -= 1;
                *front = node.next;
                Some(arena_child(document, node))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.inner {
            ChildIter::Owned(iter) => iter.size_hint(),
            ChildIter::Arena { remaining, .. } => (*remaining, Some(*remaining)),
        }
    }
}

impl DoubleEndedIterator for Children<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            ChildIter::Owned(iter) => iter.next_back().map(owned_child),
            ChildIter::Arena { document, back, remaining, .. } => {
                if *remaining == 0 {
                    return None;
                }
                let node = document.node(back.get()?);
                *remaining -= 1;
                *back = node.prev;
                Some(arena_child(document, node))
            }
        }
    }
}

impl ExactSizeIterator for Children<'_> {}

impl<'a> ElementRef<'a> {
    /// A handle to the element in `slot` of `document` (which must hold an
    /// element).
    pub(crate) fn in_document(document: &'a Document, slot: Slot) -> Self {
        debug_assert!(matches!(document.node(slot).data, Data::Element { .. }));
        ElementRef { inner: Inner::Arena(document, document.node(slot)) }
    }

    pub fn tag(self) -> &'a str {
        match self.inner {
            Inner::Owned(e) => &e.tag,
            Inner::Arena(d, n) => match &n.data {
                Data::Element { tag, .. } => d.atoms.resolve_name(tag),
                Data::Text(_) | Data::Free => "",
            },
        }
    }

    /// Milestone 40b: whether the element's tag is the well-known `tag` —
    /// one integer comparison (ADR 021 §5).
    #[inline]
    pub fn is(self, tag: Atom) -> bool {
        match self.inner {
            Inner::Owned(e) => e.tag.is(tag),
            Inner::Arena(_, n) => n.tag_atom() == Some(tag),
        }
    }

    /// The element's tag as a well-known atom (`None` for any other tag).
    #[inline]
    pub fn well_known_tag(self) -> Option<Atom> {
        match self.inner {
            Inner::Owned(e) => e.tag.atom(),
            Inner::Arena(_, n) => n.tag_atom().filter(|a| a.static_index().is_some()),
        }
    }

    /// Whether the element's tag is `tag`: atoms compared when both are
    /// atoms (a selector's well-known type against a well-known tag), text
    /// otherwise.
    #[inline]
    pub fn has_tag(self, tag: &Name) -> bool {
        match (tag.atom(), self.inner) {
            (Some(atom), _) => self.is(atom),
            // A name that is not well-known never names a well-known tag.
            (None, Inner::Arena(_, n)) if n.tag_atom().is_some_and(|a| a.static_index().is_some()) => false,
            (None, _) => self.tag() == tag.as_str(),
        }
    }

    pub fn node_id(self) -> NodeId {
        match self.inner {
            Inner::Owned(e) => e.node_id,
            Inner::Arena(_, n) => n.id,
        }
    }

    /// The value of the attribute `name`, if present.
    pub fn attr(self, name: &str) -> Option<&'a str> {
        match self.inner {
            Inner::Owned(e) => e.attr(name),
            Inner::Arena(d, n) => n.attr(&d.atoms, name),
        }
    }

    /// Milestone 40b: the attribute named by the well-known atom `name` —
    /// an integer comparison per attribute (ADR 021 §5).
    #[inline]
    pub fn attr_atom(self, name: Atom) -> Option<&'a str> {
        match self.inner {
            Inner::Owned(e) => e.attr_atom(name),
            Inner::Arena(_, n) => n.attr_atom(name),
        }
    }

    /// The attribute named `name`: by atom when it is well-known, by text
    /// otherwise (a selector's `[data-x]`).
    #[inline]
    pub fn attr_named(self, name: &Name) -> Option<&'a str> {
        match name.atom() {
            Some(atom) => self.attr_atom(atom),
            None => self.attr(name.as_str()),
        }
    }

    pub fn has_attr(self, name: &str) -> bool {
        self.attr(name).is_some()
    }

    /// Every attribute, in source order.
    pub fn attributes(self) -> Attributes<'a> {
        let (atoms, list): (Option<&'a names::AtomTable>, &'a [(Name, String)]) = match self.inner {
            Inner::Owned(e) => (None, &e.attributes),
            Inner::Arena(d, n) => match &n.data {
                Data::Element { attributes, .. } => (Some(&d.atoms), attributes),
                Data::Text(_) | Data::Free => (None, &[]),
            },
        };
        Attributes { atoms, list: list.iter() }
    }

    /// The element and text children, in document order.
    pub fn children(self) -> Children<'a> {
        Children {
            inner: match self.inner {
                Inner::Owned(e) => ChildIter::Owned(e.children.iter()),
                Inner::Arena(d, node) => ChildIter::Arena { document: d, front: node.first_child, back: node.last_child, remaining: node.child_count as usize },
            },
        }
    }

    /// Milestone 39c: the parent element, in O(1) (`None` for the root, and
    /// for an owned element, which has no parent link).
    pub fn parent(self) -> Option<ElementRef<'a>> {
        match self.inner {
            Inner::Owned(_) => None,
            Inner::Arena(d, n) => n.parent.get().map(|slot| ElementRef::in_document(d, slot)),
        }
    }

    /// Milestone 39c: the document this element is in (`None` for an owned
    /// element).
    pub fn document(self) -> Option<&'a Document> {
        match self.inner {
            Inner::Owned(_) => None,
            Inner::Arena(d, _) => Some(d),
        }
    }

    /// The element children only, in document order.
    pub fn element_children(self) -> impl DoubleEndedIterator<Item = ElementRef<'a>> + 'a {
        self.children().filter_map(ChildRef::as_element)
    }

    pub fn has_children(self) -> bool {
        self.child_count() > 0
    }

    /// Number of children (elements and text).
    pub fn child_count(self) -> usize {
        self.children().len()
    }

    /// This element and its descendant elements, in document order.
    pub fn descendants_and_self(self) -> impl Iterator<Item = ElementRef<'a>> + 'a {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let el = stack.pop()?;
            stack.extend(el.element_children().rev());
            Some(el)
        })
    }

    /// Concatenation of every descendant text node — `element.textContent`.
    pub fn text_content(self) -> String {
        let mut out = String::new();
        let mut stack: Vec<ChildRef<'a>> = self.children().rev().collect();
        while let Some(child) = stack.pop() {
            match child {
                ChildRef::Text(t) => out.push_str(t),
                ChildRef::Element(el) => stack.extend(el.children().rev()),
            }
        }
        out
    }

    /// The first element in this subtree (itself included), in document
    /// order, whose `id` is `id`. A walk: a live document answers this from
    /// its index instead ([`Document::find_by_id`]).
    pub fn find_by_id(self, id: &str) -> Option<ElementRef<'a>> {
        self.descendants_and_self().find(|e| e.attr_atom(crate::atoms::ID) == Some(id))
    }

    /// The element with `node_id` in this subtree (itself included).
    pub fn find_by_node_id(self, node_id: NodeId) -> Option<ElementRef<'a>> {
        match self.inner {
            // In a document: an index lookup and an ancestry check.
            Inner::Arena(d, _) => d.find_by_node_id(node_id).filter(|_| d.is_inclusive_ancestor(self.node_id(), node_id)),
            Inner::Owned(_) => self.descendants_and_self().find(|e| e.node_id() == node_id),
        }
    }

    /// `document.querySelector` for one compound selector — `tag`,
    /// `.class`, `#id`, or `tag.class`/`tag#id` combinations, no
    /// combinators — the first match in document order. Real selector
    /// matching lives in `style`; `dom` does not depend on it.
    pub fn query_selector(self, selector: &str) -> Option<ElementRef<'a>> {
        let selector = selector.trim();
        self.descendants_and_self().find(|e| e.matches_simple_selector(selector))
    }

    fn matches_simple_selector(self, selector: &str) -> bool {
        if let Some(id) = selector.strip_prefix('#') {
            return self.attr_atom(crate::atoms::ID) == Some(id);
        }
        if let Some(class) = selector.strip_prefix('.') {
            return self.attr_atom(crate::atoms::CLASS).is_some_and(|c| c.split_whitespace().any(|w| w == class));
        }
        let split_at = selector.find(['.', '#']).unwrap_or(selector.len());
        let (tag_part, rest) = selector.split_at(split_at);
        if !tag_part.is_empty() && self.tag() != tag_part {
            return false;
        }
        rest.is_empty() || self.matches_simple_selector(rest)
    }

    /// Is this element text-editable: a text-like `<input>` (not `submit`,
    /// `checkbox` or `radio`), a `<textarea>`, or a `contenteditable`
    /// element (bare attribute or any value but a literal `"false"`, per
    /// HTML's boolean-attribute convention; the inherited tri-state is not
    /// modelled). The ONE definition — native editing in `js`, and caret,
    /// click-to-position and tab order in `engine`, all ask here (Milestone
    /// 29).
    pub fn is_text_editable(self) -> bool {
        match self.tag() {
            "input" => !matches!(self.attr_atom(crate::atoms::TYPE), Some("submit" | "checkbox" | "radio")),
            "textarea" => true,
            _ => self.attr_atom(crate::atoms::CONTENTEDITABLE).is_some_and(|v| v != "false"),
        }
    }

    /// The current editable text of a text-editable element: the `value`
    /// attribute for `<input>`/`<textarea>` (this engine's simplified value
    /// model), the text content for a `contenteditable` element.
    pub fn editable_value(self) -> String {
        match self.tag() {
            "input" | "textarea" => self.attr_atom(crate::atoms::VALUE).unwrap_or("").to_string(),
            _ => self.text_content(),
        }
    }

    /// Whether a form field satisfies its constraints: an `<input>` or
    /// `<textarea>` that is `required` has a value, and a non-empty value
    /// matches its `pattern` (with the linear-time matcher, ADR 003; an
    /// unusable pattern imposes nothing). Anything else is valid. The ONE
    /// definition (ADR 021 §6): `:valid`/`:invalid` in `style`, the
    /// submission gate in `js` and the `invalid` state in `accessibility`
    /// all ask here — they were three copies (Milestones 20 and 24).
    pub fn is_valid(self) -> bool {
        if !matches!(self.tag(), "input" | "textarea") {
            return true;
        }
        let value = self.attr_atom(crate::atoms::VALUE).unwrap_or("");
        if self.attr_atom(crate::atoms::REQUIRED).is_some() && value.is_empty() {
            return false;
        }
        match self.attr_atom(crate::atoms::PATTERN) {
            Some(pattern) if !value.is_empty() => pattern::matches(pattern, value) != Some(false),
            _ => true,
        }
    }

    /// A deep copy as an owned fragment, with the same element `NodeId`s.
    /// Iterative.
    pub fn to_element(self) -> Element {
        struct Frame<'a> {
            element: Element,
            children: Children<'a>,
        }
        // Names leave the document as text unless well-known (a fragment
        // has no table).
        let shell = |e: ElementRef<'_>| Element {
            tag: Name::new(e.tag()),
            attributes: e.attributes().map(|(k, v)| (Name::new(k), v.to_string())).collect(),
            children: Vec::new(),
            node_id: e.node_id(),
        };
        let mut stack = vec![Frame { element: shell(self), children: self.children() }];
        loop {
            let top = stack.last_mut().expect("the root frame is popped last");
            match top.children.next() {
                Some(ChildRef::Text(t)) => top.element.children.push(Node::Text(t.to_string())),
                Some(ChildRef::Element(e)) => stack.push(Frame { element: shell(e), children: e.children() }),
                None => {
                    let done = stack.pop().expect("non-empty").element;
                    match stack.last_mut() {
                        Some(parent) => parent.element.children.push(Node::Element(done)),
                        None => return done,
                    }
                }
            }
        }
    }
}

// Readers run on `rayon` workers (parallel style resolution): the handle
// must cross threads.
const _: () = {
    const fn assert_shared<T: Send + Sync>() {}
    assert_shared::<ElementRef<'static>>();
    assert_shared::<ChildRef<'static>>();
};
