//! A live document — Milestone 31 (`docs/adr/008-dirty-tracking-and-identity.md`),
//! stored as an arena since Milestone 39 (`docs/adr/021-arena-dom.md`).
//!
//! [`Document`] owns its nodes and is the only way to change them. Nodes live
//! in slots of one `Vec`, linked to their parent, first and last child and
//! previous and next sibling by slot index, so traversal never looks anything
//! up. Every mutation goes through a method here, which bumps
//! [`Document::generation`], appends a [`MutationRecord`] for the incremental
//! pipeline to consume, and keeps two indexes up to date — `NodeId → slot` and
//! `id` attribute → elements — so that:
//!
//! - an unrecorded change cannot be written (there is no `&mut Element` access
//!   to a live document), and
//! - `find_by_node_id` is O(1) and `find_by_id` O(1) for a unique `id`.
//!
//! Readers see the document through [`ElementRef`] handles. Owned [`Element`]
//! trees are fragments: the parser, `createElement` and tests build them, and
//! [`Document::new`] and [`Document::append_child`] move them in, keeping their
//! `NodeId`s.

use crate::{Element, ElementRef, Node, NodeId};
use names::{atoms, Atom, AtomTable, Name};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A hasher for `NodeId` keys: one multiply, spread by the high bits. Safe
/// as a weak hash because ids are handed out by the engine in sequence —
/// page content cannot choose them to collide (unlike SipHash's job).
/// Milestone 39b: every map keyed by node id uses it (`style`'s cache,
/// `layout`'s reuse bookkeeping), not only the document's.
#[derive(Default)]
pub struct NodeIdHasher(u64);

impl Hasher for NodeIdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, id: u64) {
        self.0 = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }
}

/// A map keyed by node id, hashed with [`NodeIdHasher`].
pub type NodeIdMap<V> = HashMap<NodeId, V, BuildHasherDefault<NodeIdHasher>>;
/// A set of node ids, hashed with [`NodeIdHasher`].
pub type NodeIdSet = std::collections::HashSet<NodeId, BuildHasherDefault<NodeIdHasher>>;

/// Most mutation records kept between two drains. Past it the records are
/// replaced by a single "everything changed" flag: a script mutating in a
/// tight loop costs bounded memory, and the next frame simply does a full
/// restyle and relayout.
pub const MAX_MUTATION_RECORDS: usize = 10_000;

/// What kind of change a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutationKind {
    /// The target's own attributes changed (including `style`, `value`,
    /// `checked`, `class`, `id`).
    Attributes,
    /// The target's children changed (appended, or replaced by text).
    ChildList,
    /// Milestone 39: the target was inserted or moved, with its subtree —
    /// which may have been styled before, somewhere else (a removed node
    /// put back, a moved one). Recorded beside the parent's `ChildList`.
    Inserted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MutationRecord {
    pub target: NodeId,
    pub kind: MutationKind,
}

/// The records drained by [`Document::take_mutations`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mutations {
    pub records: Vec<MutationRecord>,
    /// More than [`MAX_MUTATION_RECORDS`] happened: treat everything as
    /// changed.
    pub overflowed: bool,
}

impl Mutations {
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && !self.overflowed
    }
}

/// A child-index path from the root: `[2, 0]` is the root's third child's
/// first child (indices count text nodes too).
pub type NodePath = Vec<u32>;

/// A node's position in the arena.
pub(crate) type Slot = u32;

/// A link to another node's slot, or none: a `u32` with a sentinel, half
/// the size of `Option<u32>` — walks follow these, so their size is the
/// walk's memory traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Link(u32);

impl Link {
    pub(crate) const NONE: Link = Link(u32::MAX);

    pub(crate) fn get(self) -> Option<Slot> {
        (self.0 != u32::MAX).then_some(self.0)
    }
}

impl From<Option<Slot>> for Link {
    fn from(slot: Option<Slot>) -> Link {
        slot.map_or(Link::NONE, Link)
    }
}

/// What a node is.
#[derive(Debug, Clone)]
pub(crate) enum Data {
    /// Every name an atom of the document's [`AtomTable`] (ADR 021 §5).
    Element { tag: Name, attributes: Vec<(Name, String)> },
    Text(String),
    /// A free slot (listed in `Document::free`).
    Free,
}

#[derive(Debug, Clone)]
pub(crate) struct NodeData {
    pub(crate) id: NodeId,
    pub(crate) data: Data,
    pub(crate) parent: Link,
    pub(crate) first_child: Link,
    pub(crate) last_child: Link,
    pub(crate) prev: Link,
    pub(crate) next: Link,
    pub(crate) child_count: u32,
}

impl NodeData {
    fn new(id: NodeId, data: Data) -> NodeData {
        NodeData { id, data, parent: Link::NONE, first_child: Link::NONE, last_child: Link::NONE, prev: Link::NONE, next: Link::NONE, child_count: 0 }
    }

    /// The attribute named `name`, compared as text (read through the
    /// document's table; [`Self::attr_atom`] is the fast path).
    pub(crate) fn attr<'a>(&'a self, atoms: &'a AtomTable, name: &str) -> Option<&'a str> {
        match &self.data {
            Data::Element { attributes, .. } => attributes.iter().find(|(k, _)| atoms.resolve_name(k) == name).map(|(_, v)| v.as_str()),
            Data::Text(_) | Data::Free => None,
        }
    }

    /// The attribute whose name is `name`: an integer comparison each.
    #[inline]
    pub(crate) fn attr_atom(&self, name: Atom) -> Option<&str> {
        match &self.data {
            Data::Element { attributes, .. } => attributes.iter().find(|(k, _)| k.atom() == Some(name)).map(|(_, v)| v.as_str()),
            Data::Text(_) | Data::Free => None,
        }
    }

    /// The element's tag atom (`None` for a text or free slot).
    #[inline]
    pub(crate) fn tag_atom(&self) -> Option<Atom> {
        match &self.data {
            Data::Element { tag, .. } => tag.atom(),
            Data::Text(_) | Data::Free => None,
        }
    }
}

#[derive(Debug)]
pub struct Document {
    /// Every node; a free slot holds `Data::Free` (and is listed in `free`).
    nodes: Vec<NodeData>,
    free: Vec<Slot>,
    root: Slot,
    /// Every node in the document (elements and text) → its slot.
    slots: NodeIdMap<Slot>,
    /// How many of them are elements.
    elements: usize,
    /// `id` attribute value → the elements carrying it (usually one).
    ids: HashMap<String, Vec<NodeId>>,
    /// The names this document's elements use that are not well-known
    /// (ADR 021 §5): the document's own, freed with it.
    pub(crate) atoms: AtomTable,
    generation: u64,
    mutations: Mutations,
    /// Links rewritten since the document was built (parent, child and
    /// sibling pointers): what a mutation costs, for the tests that keep
    /// it O(1) (ADR 021 §7).
    link_updates: u64,
}

/// An element of a live document, opened for changing its attributes only
/// ([`Document::mutate_elements`]).
pub struct ElementMut<'a> {
    node: &'a mut NodeData,
    atoms: &'a mut AtomTable,
}

impl ElementMut<'_> {
    pub fn tag(&self) -> &str {
        match &self.node.data {
            Data::Element { tag, .. } => self.atoms.resolve_name(tag),
            Data::Text(_) | Data::Free => "",
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.node.attr(self.atoms, name)
    }

    /// The attribute named by the well-known atom `name`.
    pub fn attr_atom(&self, name: Atom) -> Option<&str> {
        self.node.attr_atom(name)
    }

    /// Whether the tag is the well-known `tag`.
    pub fn is(&self, tag: Atom) -> bool {
        self.node.tag_atom() == Some(tag)
    }

    pub fn set_attr(&mut self, name: &str, value: impl Into<String>) {
        let name = self.atoms.name(name);
        if let Data::Element { attributes, .. } = &mut self.node.data {
            set_attr_in(attributes, name, value.into());
        }
    }

    pub fn remove_attr(&mut self, name: &str) {
        let Some(name) = self.atoms.find(name) else { return }; // never seen: absent
        if let Data::Element { attributes, .. } = &mut self.node.data {
            attributes.retain(|(k, _)| k.atom() != Some(name));
        }
    }
}

/// A node that is not in a document, with its subtree: what goes into one
/// and comes out of one (ADR 021 §3, §7). An element fragment keeps its
/// elements' `NodeId`s; a text fragment keeps its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Fragment {
    Element(Element),
    Text { id: NodeId, text: String },
}

impl Fragment {
    /// A new text node — `document.createTextNode(text)`.
    pub fn text(text: impl Into<String>) -> Fragment {
        Fragment::Text { id: crate::next_node_id(), text: text.into() }
    }

    /// The node's identity.
    pub fn id(&self) -> NodeId {
        match self {
            Fragment::Element(e) => e.node_id,
            Fragment::Text { id, .. } => *id,
        }
    }
}

impl From<Element> for Fragment {
    fn from(element: Element) -> Fragment {
        Fragment::Element(element)
    }
}

/// Gives every element in `root` a fresh id (a clone is a new node).
pub(crate) fn renumber(root: &mut Element) {
    let mut stack = vec![root];
    while let Some(el) = stack.pop() {
        el.node_id = crate::next_node_id();
        stack.extend(el.children.iter_mut().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        }));
    }
}

/// Sets `name` (an atom of the document's table) in an arena element's list.
fn set_attr_in(attributes: &mut Vec<(Name, String)>, name: Name, value: String) {
    match attributes.iter_mut().find(|(k, _)| k.atom() == name.atom()) {
        Some(existing) => existing.1 = value,
        None => attributes.push((name, value)),
    }
}

/// How many nodes an owned fragment holds (to reserve room for them).
fn fragment_size(root: &Element) -> usize {
    let mut count = 1; // the root; every other node is counted as a child
    let mut stack = vec![root];
    while let Some(el) = stack.pop() {
        count += el.children.len();
        stack.extend(el.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        }));
    }
    count
}

impl Document {
    pub fn new(root: Element) -> Document {
        let size = fragment_size(&root);
        let mut document = Document {
            nodes: Vec::with_capacity(size),
            free: Vec::new(),
            root: 0,
            slots: NodeIdMap::with_capacity_and_hasher(size, Default::default()),
            elements: 0,
            ids: HashMap::new(),
            atoms: AtomTable::default(),
            generation: 0,
            mutations: Mutations::default(),
            link_updates: 0,
        };
        document.root = document.insert_fragment(root, None);
        document
    }

    pub fn root(&self) -> ElementRef<'_> {
        ElementRef::in_document(self, self.root)
    }

    /// Counts every mutation ever made; unchanged means the tree is unchanged.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Takes the records accumulated since the last call.
    pub fn take_mutations(&mut self) -> Mutations {
        std::mem::take(&mut self.mutations)
    }

    /// How many nodes have been linked into or unlinked from the tree since
    /// it was built — each a constant number of pointer writes. A mutation
    /// of one node costs one or two, whatever the size of the document.
    pub fn link_updates(&self) -> u64 {
        self.link_updates
    }

    /// Records not yet taken (for tests and diagnostics).
    pub fn pending_mutations(&self) -> &Mutations {
        &self.mutations
    }

    // ---- The arena ----

    pub(crate) fn node(&self, slot: Slot) -> &NodeData {
        &self.nodes[slot as usize]
    }

    fn node_mut(&mut self, slot: Slot) -> &mut NodeData {
        &mut self.nodes[slot as usize]
    }

    fn element_slot(&self, node: NodeId) -> Option<Slot> {
        let slot = *self.slots.get(&node)?;
        matches!(self.node(slot).data, Data::Element { .. }).then_some(slot)
    }

    fn allocate(&mut self, node: NodeData) -> Slot {
        let id = node.id;
        if let Data::Element { attributes, .. } = &node.data {
            self.elements += 1;
            if let Some((_, value)) = attributes.iter().find(|(k, _)| k.is(atoms::ID)) {
                self.ids.entry(value.clone()).or_default().push(id);
            }
        }
        let slot = match self.free.pop() {
            Some(slot) => {
                self.nodes[slot as usize] = node;
                slot
            }
            None => {
                self.nodes.push(node);
                (self.nodes.len() - 1) as Slot
            }
        };
        self.slots.insert(id, slot);
        slot
    }

    /// Links the unattached `child` into `parent`'s children just before
    /// `before` (one of them), or last. O(1).
    fn link_before(&mut self, parent: Slot, child: Slot, before: Option<Slot>) {
        let Some(before) = before else { return self.link_last(parent, child) };
        self.link_updates += 1;
        let prev = self.node(before).prev;
        {
            let c = self.node_mut(child);
            c.parent = Some(parent).into();
            c.prev = prev;
            c.next = Some(before).into();
        }
        match prev.get() {
            Some(prev) => self.node_mut(prev).next = Some(child).into(),
            None => self.node_mut(parent).first_child = Some(child).into(),
        }
        self.node_mut(before).prev = Some(child).into();
        self.node_mut(parent).child_count += 1;
    }

    /// Links the unattached `child` as `parent`'s last child.
    fn link_last(&mut self, parent: Slot, child: Slot) {
        self.link_updates += 1;
        let last = self.node(parent).last_child;
        {
            let c = self.node_mut(child);
            c.parent = Some(parent).into();
            c.prev = last;
            c.next = Link::NONE;
        }
        match last.get() {
            Some(last) => self.node_mut(last).next = Some(child).into(),
            None => self.node_mut(parent).first_child = Some(child).into(),
        }
        let p = self.node_mut(parent);
        p.last_child = Some(child).into();
        p.child_count += 1;
    }

    /// Moves an owned fragment into the arena (as `parent`'s last child, if
    /// any), keeping its elements' `NodeId`s; text nodes get fresh ones.
    /// Iterative: a script-built fragment can be arbitrarily deep.
    fn insert_fragment(&mut self, mut root: Element, at: Option<(Slot, Option<Slot>)>) -> Slot {
        // Moves the names in and interns the fragment's own ones in place
        // (ADR 021 §5): well-known names, the usual case, are already atoms,
        // and the attribute list is moved, not rebuilt. (`Element` has a
        // `Drop`, so its fields are swapped out; the placeholder tag is an
        // atom and costs nothing.)
        let take = |atoms: &mut AtomTable, el: &mut Element| {
            let mut tag = std::mem::replace(&mut el.tag, Name::well_known(names::atoms::HTML));
            let mut attributes = std::mem::take(&mut el.attributes);
            atoms.intern_name(&mut tag);
            for (name, _) in &mut attributes {
                atoms.intern_name(name);
            }
            (tag, attributes, std::mem::take(&mut el.children))
        };
        let (tag, attributes, children) = take(&mut self.atoms, &mut root);
        let root_slot = self.allocate(NodeData::new(root.node_id, Data::Element { tag, attributes }));
        if let Some((parent, before)) = at {
            self.link_before(parent, root_slot, before);
        }
        let mut work: Vec<(Slot, Vec<Node>)> = vec![(root_slot, children)];
        while let Some((slot, children)) = work.pop() {
            for child in children {
                match child {
                    Node::Text(text) => {
                        let t = self.allocate(NodeData::new(crate::next_node_id(), Data::Text(text)));
                        self.link_last(slot, t);
                    }
                    Node::Element(mut el) => {
                        let (tag, attributes, grandchildren) = take(&mut self.atoms, &mut el);
                        let c = self.allocate(NodeData::new(el.node_id, Data::Element { tag, attributes }));
                        self.link_last(slot, c);
                        work.push((c, grandchildren));
                    }
                }
            }
        }
        root_slot
    }

    /// Unlinks `slot` from its parent and siblings (the subtree stays intact).
    fn unlink(&mut self, slot: Slot) {
        self.link_updates += 1;
        let (parent, prev, next) = {
            let n = self.node(slot);
            (n.parent.get(), n.prev, n.next)
        };
        match prev.get() {
            Some(p) => self.node_mut(p).next = next,
            None => {
                if let Some(parent) = parent {
                    self.node_mut(parent).first_child = next;
                }
            }
        }
        match next.get() {
            Some(n) => self.node_mut(n).prev = prev,
            None => {
                if let Some(parent) = parent {
                    self.node_mut(parent).last_child = prev;
                }
            }
        }
        if let Some(parent) = parent {
            self.node_mut(parent).child_count -= 1;
        }
        let n = self.node_mut(slot);
        n.parent = Link::NONE;
        n.prev = Link::NONE;
        n.next = Link::NONE;
    }

    /// Frees `slot` and its whole subtree: slots back to the free list,
    /// out of both indexes. The caller has unlinked it.
    fn free_subtree(&mut self, slot: Slot) {
        let mut stack = vec![slot];
        while let Some(s) = stack.pop() {
            let node = std::mem::replace(&mut self.nodes[s as usize], NodeData::new(0, Data::Free));
            self.slots.remove(&node.id);
            if let Data::Element { attributes, .. } = &node.data {
                self.elements -= 1;
                if let Some((_, value)) = attributes.iter().find(|(k, _)| k.is(atoms::ID)) {
                    self.unindex_id(node.id, value);
                }
            }
            let mut child = node.first_child.get();
            while let Some(c) = child {
                stack.push(c);
                child = self.node(c).next.get();
            }
            self.free.push(s);
        }
    }

    fn unindex_id(&mut self, node: NodeId, value: &str) {
        if let Some(list) = self.ids.get_mut(value) {
            list.retain(|n| *n != node);
            if list.is_empty() {
                self.ids.remove(value);
            }
        }
    }

    // ---- Lookups ----

    /// `node`'s parent's id, in O(1) (`None` for the root or an unknown node).
    pub fn parent_id(&self, node: NodeId) -> Option<NodeId> {
        let slot = self.element_slot(node)?;
        Some(self.node(self.node(slot).parent.get()?).id)
    }

    /// Whether the element `node` is in this document, in O(1).
    pub fn contains(&self, node: NodeId) -> bool {
        self.element_slot(node).is_some()
    }

    /// How many elements the document holds (the root included).
    pub fn element_count(&self) -> usize {
        self.elements
    }

    /// Milestone 40b: how many names this document's own table holds — the
    /// tag and attribute names its page invented, beyond the well-known ones
    /// (ADR 021 §5). The table lives and dies with the document.
    pub fn invented_names(&self) -> usize {
        self.atoms.len()
    }

    /// Whether the element `ancestor` is the element `node` or one of its
    /// ancestors, in O(depth).
    pub fn is_inclusive_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        let (Some(target), Some(mut at)) = (self.element_slot(ancestor), self.element_slot(node)) else { return false };
        loop {
            if at == target {
                return true;
            }
            match self.node(at).parent.get() {
                Some(parent) => at = parent,
                None => return false,
            }
        }
    }

    /// `node`'s child-index path from the root (text nodes count), in
    /// O(depth × the siblings before it on each level).
    pub fn path_of(&self, node: NodeId) -> Option<NodePath> {
        let mut at = self.element_slot(node)?;
        let mut path = Vec::new();
        while let Some(parent) = self.node(at).parent.get() {
            let mut index = 0u32;
            let mut sibling = self.node(at).prev.get();
            while let Some(s) = sibling {
                index += 1;
                sibling = self.node(s).prev.get();
            }
            path.push(index);
            at = parent;
        }
        path.reverse();
        Some(path)
    }

    /// The element at `path`, if the path is valid.
    pub fn element_at(&self, path: &[u32]) -> Option<ElementRef<'_>> {
        let mut at = self.root;
        for &index in path {
            let mut child = self.node(at).first_child.get();
            for _ in 0..index {
                child = self.node(child?).next.get();
            }
            at = child?;
        }
        matches!(self.node(at).data, Data::Element { .. }).then(|| ElementRef::in_document(self, at))
    }

    /// The element with this identity, in O(1).
    pub fn find_by_node_id(&self, node: NodeId) -> Option<ElementRef<'_>> {
        self.element_slot(node).map(|slot| ElementRef::in_document(self, slot))
    }

    /// The first element in document order whose `id` is `id`: O(1) when
    /// the `id` is unique, as it almost always is.
    pub fn find_by_id(&self, id: &str) -> Option<ElementRef<'_>> {
        let candidates = self.ids.get(id)?;
        let first = match candidates.as_slice() {
            [only] => *only,
            _ => candidates.iter().filter_map(|n| self.path_of(*n).map(|p| (p, *n))).min_by(|a, b| a.0.cmp(&b.0))?.1,
        };
        self.find_by_node_id(first)
    }

    /// The node id of `find_by_id(id)`.
    pub fn node_id_of(&self, id: &str) -> Option<NodeId> {
        self.find_by_id(id).map(|e| e.node_id())
    }

    /// The ancestors of `node`, closest first (empty for the root).
    pub fn ancestors_of(&self, node: NodeId) -> Vec<ElementRef<'_>> {
        let mut chain = Vec::new();
        let Some(mut at) = self.element_slot(node) else { return chain };
        while let Some(parent) = self.node(at).parent.get() {
            chain.push(ElementRef::in_document(self, parent));
            at = parent;
        }
        chain
    }

    /// The parent of `node`, if it has one in this document.
    pub fn parent_of(&self, node: NodeId) -> Option<ElementRef<'_>> {
        let slot = self.element_slot(node)?;
        self.node(slot).parent.get().map(|p| ElementRef::in_document(self, p))
    }

    // ---- Mutations ----

    fn record(&mut self, target: NodeId, kind: MutationKind) {
        self.generation += 1;
        if self.mutations.overflowed {
            return;
        }
        if self.mutations.records.len() >= MAX_MUTATION_RECORDS {
            self.mutations.records = Vec::new();
            self.mutations.overflowed = true;
        } else {
            self.mutations.records.push(MutationRecord { target, kind });
        }
    }

    /// Records that `node` was inserted or moved — part of the mutation the
    /// parent's `ChildList` already counted, so the generation stays.
    fn record_inserted(&mut self, node: NodeId) {
        self.generation -= 1;
        self.record(node, MutationKind::Inserted);
    }

    fn reindex_id(&mut self, node: NodeId, old: Option<String>, new: Option<&str>) {
        if old.as_deref() == new {
            return;
        }
        if let Some(old) = old {
            self.unindex_id(node, &old);
        }
        if let Some(new) = new {
            self.ids.entry(new.to_string()).or_default().push(node);
        }
    }

    fn attributes_mut(&mut self, slot: Slot) -> &mut Vec<(Name, String)> {
        match &mut self.node_mut(slot).data {
            Data::Element { attributes, .. } => attributes,
            Data::Text(_) | Data::Free => unreachable!("element_slot only returns elements"),
        }
    }

    /// Sets an attribute. `false` if `node` is not in this document.
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: impl Into<String>) -> bool {
        let value = value.into();
        let Some(slot) = self.element_slot(node) else { return false };
        // `setAttribute` is where a script's names are born: interned here.
        let name = self.atoms.name(name);
        let atom = name.atom().expect("a table's name is an atom");
        let old = self.node(slot).attr_atom(atom).map(str::to_string);
        if old.as_deref() == Some(value.as_str()) {
            return true; // no change, no record
        }
        set_attr_in(self.attributes_mut(slot), name, value.clone());
        if atom == atoms::ID {
            self.reindex_id(node, old, Some(&value));
        }
        self.record(node, MutationKind::Attributes);
        true
    }

    /// Removes an attribute (a no-op, unrecorded, if it is absent).
    pub fn remove_attribute(&mut self, node: NodeId, name: &str) -> bool {
        let Some(slot) = self.element_slot(node) else { return false };
        let Some(atom) = self.atoms.find(name) else { return true }; // never seen: absent
        let Some(old) = self.node(slot).attr_atom(atom).map(str::to_string) else { return true };
        self.attributes_mut(slot).retain(|(k, _)| k.atom() != Some(atom));
        if atom == atoms::ID {
            self.reindex_id(node, Some(old), None);
        }
        self.record(node, MutationKind::Attributes);
        true
    }

    /// `element.style.<property> = value`.
    pub fn set_inline_style_property(&mut self, node: NodeId, property: &str, value: &str) -> bool {
        let Some(slot) = self.element_slot(node) else { return false };
        let before = self.node(slot).attr_atom(atoms::STYLE).map(str::to_string);
        let after = crate::rewrite_inline_style(before.as_deref(), property, value);
        if before.as_deref() != Some(after.as_str()) {
            set_attr_in(self.attributes_mut(slot), Name::well_known(atoms::STYLE), after);
            self.record(node, MutationKind::Attributes);
        }
        true
    }

    /// Replaces all of `node`'s children with one text node.
    pub fn set_text_content(&mut self, node: NodeId, text: &str) -> bool {
        let Some(slot) = self.element_slot(node) else { return false };
        let only_text = {
            let n = self.node(slot);
            n.child_count == 1 && n.first_child.get().is_some_and(|c| matches!(&self.node(c).data, Data::Text(t) if t == text))
        };
        if only_text {
            return true;
        }
        while let Some(child) = self.node(slot).first_child.get() {
            self.unlink(child);
            self.free_subtree(child);
        }
        let t = self.allocate(NodeData::new(crate::next_node_id(), Data::Text(text.to_string())));
        self.link_last(slot, t);
        self.record(node, MutationKind::ChildList);
        true
    }

    /// Milestone 40: sets the text of the text node `node` (`text.textContent
    /// = …` in script), recorded as a change to its parent's children.
    pub fn set_text(&mut self, node: NodeId, text: &str) -> bool {
        let Some(&slot) = self.slots.get(&node) else { return false };
        let Data::Text(current) = &mut self.node_mut(slot).data else { return false };
        if current == text {
            return true;
        }
        *current = text.to_string();
        if let Some(parent) = self.node(slot).parent.get().map(|p| self.node(p).id) {
            self.record(parent, MutationKind::ChildList);
        }
        true
    }

    /// Appends `child` (a detached element, with any subtree) to `parent`.
    /// Gives it back if `parent` is not in this document.
    pub fn append_child(&mut self, parent: NodeId, child: Element) -> Result<(), Element> {
        match self.insert_before(parent, Fragment::Element(child), None) {
            Ok(()) => Ok(()),
            Err(Fragment::Element(child)) => Err(child),
            Err(Fragment::Text { .. }) => unreachable!("an element was given"),
        }
    }

    /// `parent.insertBefore(node, reference)` (`reference` `None`: append) for
    /// a node not in the document. O(1) link updates, plus building the
    /// fragment's slots. Gives the fragment back if `parent` is not an
    /// element here or `reference` is not one of its children.
    pub fn insert_before(&mut self, parent: NodeId, child: Fragment, reference: Option<NodeId>) -> Result<(), Fragment> {
        let Some(parent_slot) = self.element_slot(parent) else { return Err(child) };
        let before = match reference {
            None => None,
            Some(r) => match self.child_slot(parent_slot, r) {
                Some(slot) => Some(slot),
                None => return Err(child),
            },
        };
        let id = child.id();
        if self.slots.contains_key(&id) {
            return Err(child); // already in this document: `move_before` moves it
        }
        match child {
            Fragment::Element(element) => {
                self.insert_fragment(element, Some((parent_slot, before)));
            }
            Fragment::Text { id, text } => {
                let slot = self.allocate(NodeData::new(id, Data::Text(text)));
                self.link_before(parent_slot, slot, before);
            }
        }
        self.record(parent, MutationKind::ChildList);
        self.record_inserted(id);
        Ok(())
    }

    /// `parent.insertBefore(node, reference)` / `appendChild(node)` for a node
    /// already in the document: it moves, with its subtree, by relinking —
    /// O(1). `false` (nothing changes) if the move is not allowed: `node` is
    /// the root, `parent` itself or one of its ancestors, or `reference` is
    /// not a child of `parent`.
    pub fn move_before(&mut self, parent: NodeId, node: NodeId, reference: Option<NodeId>) -> bool {
        let (Some(parent_slot), Some(&slot)) = (self.element_slot(parent), self.slots.get(&node)) else { return false };
        if slot == self.root || self.is_inclusive_ancestor(node, parent) || reference == Some(node) {
            return false;
        }
        let before = match reference {
            None => None,
            Some(r) => match self.child_slot(parent_slot, r) {
                Some(s) => Some(s),
                None => return false,
            },
        };
        let old_parent = self.node(slot).parent.get().map(|p| self.node(p).id);
        self.unlink(slot);
        self.link_before(parent_slot, slot, before);
        if let Some(old_parent) = old_parent.filter(|p| *p != parent) {
            self.record(old_parent, MutationKind::ChildList);
        }
        self.record(parent, MutationKind::ChildList);
        self.record_inserted(node);
        true
    }

    /// `parent.removeChild(child)`: the child leaves the document with its
    /// subtree, as a fragment keeping its element ids (a script's handle to
    /// it stays valid and can insert it again). `None` if `child` is not a
    /// child of `parent`.
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Option<Fragment> {
        let parent_slot = self.element_slot(parent)?;
        let slot = self.child_slot(parent_slot, child)?;
        let fragment = self.fragment_of(slot);
        self.unlink(slot);
        self.free_subtree(slot);
        self.record(parent, MutationKind::ChildList);
        Some(fragment)
    }

    /// `parent.replaceChild(new, old)`: `new` takes `old`'s place; `old` is
    /// given back as a fragment. Gives `new` back if `old` is not a child of
    /// `parent`.
    pub fn replace_child(&mut self, parent: NodeId, new: Fragment, old: NodeId) -> Result<Fragment, Fragment> {
        self.insert_before(parent, new, Some(old))?;
        Ok(self.remove_child(parent, old).expect("`old` was checked to be a child of `parent`"))
    }

    /// `node.cloneNode(deep)`: a copy with fresh ids (a clone is a new
    /// node), not in the document. `deep: false` copies an element's
    /// attributes and none of its children.
    pub fn clone_node(&self, node: NodeId, deep: bool) -> Option<Fragment> {
        let slot = *self.slots.get(&node)?;
        Some(match &self.node(slot).data {
            Data::Element { tag, attributes } => Fragment::Element(if deep {
                ElementRef::in_document(self, slot).to_element().clone_node(true)
            } else {
                let attributes = attributes.iter().map(|(k, v)| (self.atoms.detach(k), v.clone())).collect();
                Element { tag: self.atoms.detach(tag), attributes, children: Vec::new(), node_id: 0 }.clone_node(false)
            }),
            Data::Text(text) => Fragment::text(text.clone()),
            Data::Free => return None,
        })
    }

    /// Whether `node` — an element or a text node — is in this document.
    pub fn contains_node(&self, node: NodeId) -> bool {
        self.slots.contains_key(&node)
    }

    /// The parent of any node (element or text), by id.
    pub fn parent_node_id(&self, node: NodeId) -> Option<NodeId> {
        let slot = *self.slots.get(&node)?;
        Some(self.node(self.node(slot).parent.get()?).id)
    }

    /// The text of the text node `node`, if it is one in this document.
    pub fn text_of(&self, node: NodeId) -> Option<&str> {
        match &self.node(*self.slots.get(&node)?).data {
            Data::Text(t) => Some(t),
            _ => None,
        }
    }

    /// `child`'s slot, if it is a child (of any kind) of `parent_slot`.
    fn child_slot(&self, parent_slot: Slot, child: NodeId) -> Option<Slot> {
        let slot = *self.slots.get(&child)?;
        (self.node(slot).parent.get() == Some(parent_slot)).then_some(slot)
    }

    /// The node in `slot` and its subtree, copied out as a fragment.
    fn fragment_of(&self, slot: Slot) -> Fragment {
        match &self.node(slot).data {
            Data::Element { .. } => Fragment::Element(ElementRef::in_document(self, slot).to_element()),
            Data::Text(text) => Fragment::Text { id: self.node(slot).id, text: text.clone() },
            Data::Free => unreachable!("a linked slot is never free"),
        }
    }

    /// Applies `change` to every element in document order; `change`
    /// returns whether it modified that element's attributes. Each modified
    /// element is recorded (and re-indexed if its `id` changed). For
    /// document-wide attribute updates such as un-checking a radio group.
    pub fn mutate_elements(&mut self, mut change: impl FnMut(&mut ElementMut<'_>) -> bool) {
        let mut changed: Vec<(NodeId, Option<String>, Option<String>)> = Vec::new();
        let mut stack = vec![self.root];
        while let Some(slot) = stack.pop() {
            let node = &mut self.nodes[slot as usize];
            if matches!(node.data, Data::Element { .. }) {
                let old_id = node.attr_atom(atoms::ID).map(str::to_string);
                let mut element = ElementMut { node, atoms: &mut self.atoms };
                if change(&mut element) {
                    changed.push((element.node.id, old_id, element.node.attr_atom(atoms::ID).map(str::to_string)));
                }
            }
            // Children pushed last-first, so they pop in document order.
            let mut child = self.node(slot).last_child.get();
            while let Some(c) = child {
                stack.push(c);
                child = self.node(c).prev.get();
            }
        }
        for (node, old_id, new_id) in changed {
            self.reindex_id(node, old_id, new_id.as_deref());
            self.record(node, MutationKind::Attributes);
        }
    }

    /// Checks the arena's links and both indexes against a fresh walk of
    /// the tree (tests; cheap enough for debug assertions too).
    pub fn index_is_consistent(&self) -> bool {
        let mut seen_slots: NodeIdMap<Slot> = NodeIdMap::default();
        let mut ids: HashMap<String, Vec<NodeId>> = HashMap::new();
        let (mut elements, mut live) = (0usize, 0usize);
        if self.node(self.root).parent != Link::NONE {
            return false;
        }
        let mut stack = vec![self.root];
        while let Some(slot) = stack.pop() {
            let Some(node) = self.nodes.get(slot as usize) else { return false };
            live += 1;
            if seen_slots.insert(node.id, slot).is_some() {
                return false; // a node reachable twice
            }
            match node.data {
                Data::Free => return false, // a freed slot still linked
                Data::Element { .. } => {
                    elements += 1;
                    if let Some(value) = node.attr_atom(atoms::ID) {
                        ids.entry(value.to_string()).or_default().push(node.id);
                    }
                }
                Data::Text(_) => {}
            }
            // The children's links agree with the parent's and each other's.
            let (mut count, mut prev, mut child) = (0u32, Link::NONE, node.first_child.get());
            while let Some(c) = child {
                let Some(cn) = self.nodes.get(c as usize) else { return false };
                if cn.parent.get() != Some(slot) || cn.prev != prev {
                    return false;
                }
                count += 1;
                prev = Some(c).into();
                stack.push(c);
                child = cn.next.get();
            }
            if node.last_child != prev || node.child_count != count {
                return false;
            }
        }
        let normalize = |m: &HashMap<String, Vec<NodeId>>| {
            let mut v: Vec<(String, Vec<NodeId>)> = m.iter().map(|(k, v)| (k.clone(), { let mut v = v.clone(); v.sort(); v })).collect();
            v.sort();
            v
        };
        let free_marked = self.free.iter().all(|&s| matches!(self.nodes.get(s as usize).map(|n| &n.data), Some(Data::Free)));
        seen_slots == self.slots && elements == self.elements && normalize(&ids) == normalize(&self.ids) && free_marked && live + self.free.len() == self.nodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(tag: &str, id: Option<&str>) -> Element {
        let mut e = Element::new(tag);
        if let Some(id) = id {
            e.set_attr("id", id);
        }
        e
    }

    fn sample() -> Document {
        let mut root = el("body", Some("root"));
        let mut a = el("div", Some("a"));
        a.children.push(Node::Text("hello".into()));
        a.append_child(el("span", Some("a-child")));
        root.append_child(a);
        root.children.push(Node::Text("between".into()));
        root.append_child(el("p", Some("b")));
        Document::new(root)
    }

    #[test]
    fn lookups_follow_the_index() {
        let doc = sample();
        assert_eq!(doc.find_by_id("a-child").map(|e| e.tag()), Some("span"));
        assert_eq!(doc.find_by_id("b").map(|e| e.tag()), Some("p"));
        assert_eq!(doc.path_of(doc.node_id_of("b").unwrap()), Some(vec![2u32]), "text nodes count in paths");
        assert!(doc.find_by_id("missing").is_none());
        let child = doc.node_id_of("a-child").unwrap();
        let chain: Vec<&str> = doc.ancestors_of(child).iter().map(|e| e.attr("id").unwrap()).collect();
        assert_eq!(chain, vec!["a", "root"]);
        assert_eq!(doc.parent_of(child).and_then(|e| e.attr("id")), Some("a"));
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn every_mutation_is_recorded_and_bumps_the_generation() {
        let mut doc = sample();
        let (a, b) = (doc.node_id_of("a").unwrap(), doc.node_id_of("b").unwrap());
        let g0 = doc.generation();
        assert!(doc.set_attribute(a, "class", "x"));
        assert!(doc.set_attribute(a, "class", "x"), "same value");
        assert!(doc.remove_attribute(b, "nope"), "absent attribute");
        assert!(doc.set_inline_style_property(b, "color", "red"));
        assert!(doc.set_text_content(b, "text"));
        let i = el("i", None);
        let inserted = i.node_id;
        assert!(doc.append_child(b, i).is_ok());
        assert_eq!(doc.generation(), g0 + 4, "no-op changes are not mutations");
        let m = doc.take_mutations();
        assert_eq!(
            m.records,
            vec![
                MutationRecord { target: a, kind: MutationKind::Attributes },
                MutationRecord { target: b, kind: MutationKind::Attributes },
                MutationRecord { target: b, kind: MutationKind::ChildList },
                MutationRecord { target: b, kind: MutationKind::ChildList },
                // Milestone 39: the appended node itself, for restyling.
                MutationRecord { target: inserted, kind: MutationKind::Inserted },
            ]
        );
        assert!(doc.take_mutations().is_empty(), "taken");
    }

    #[test]
    fn the_index_survives_append_removal_id_changes_and_replacement() {
        let mut doc = sample();
        let a = doc.node_id_of("a").unwrap();
        // appendChild of a detached subtree with ids inside it
        let mut sub = el("section", Some("s"));
        sub.append_child(el("em", Some("deep")));
        doc.append_child(a, sub).unwrap();
        assert_eq!(doc.find_by_id("deep").map(|e| e.tag()), Some("em"));
        assert!(doc.index_is_consistent());
        // id change
        doc.set_attribute(a, "id", "renamed");
        assert!(doc.find_by_id("a").is_none());
        assert_eq!(doc.find_by_id("renamed").map(|e| e.node_id()), Some(a));
        assert!(doc.index_is_consistent());
        doc.remove_attribute(a, "id");
        assert!(doc.find_by_id("renamed").is_none());
        assert!(doc.index_is_consistent());
        // removal (textContent replaces the subtree)
        doc.set_text_content(a, "gone");
        assert!(doc.find_by_id("deep").is_none() && doc.find_by_id("a-child").is_none() && doc.find_by_id("s").is_none());
        assert!(doc.index_is_consistent());
        // mutate_elements that renames ids
        doc.mutate_elements(|e| {
            if e.attr("id") == Some("b") {
                e.set_attr("id", "b2");
                true
            } else {
                false
            }
        });
        assert!(doc.find_by_id("b").is_none());
        assert_eq!(doc.find_by_id("b2").map(|e| e.tag()), Some("p"));
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn duplicate_ids_resolve_to_the_first_in_document_order() {
        let mut root = el("body", None);
        root.append_child(el("div", Some("dup")));
        let later = el("p", Some("dup"));
        let later_id = later.node_id;
        root.append_child(later);
        let mut doc = Document::new(root);
        let body = doc.root().node_id();
        let first = el("span", Some("dup"));
        doc.append_child(body, first).unwrap();
        assert_eq!(doc.find_by_id("dup").map(|e| e.tag()), Some("div"));
        let div = doc.node_id_of("dup").unwrap();
        doc.remove_attribute(div, "id");
        assert_eq!(doc.node_id_of("dup"), Some(later_id), "then the next one in document order");
    }

    #[test]
    fn a_mutation_storm_is_bounded() {
        let mut doc = sample();
        let a = doc.node_id_of("a").unwrap();
        for i in 0..(MAX_MUTATION_RECORDS * 3) {
            doc.set_attribute(a, "data-n", i.to_string());
        }
        let m = doc.pending_mutations();
        assert!(m.overflowed && m.records.is_empty(), "collapsed to 'everything changed'");
        assert_eq!(doc.generation(), (MAX_MUTATION_RECORDS * 3) as u64);
    }

    #[test]
    fn lookups_are_independent_of_document_size() {
        // 100,000 elements; the lookup follows a path, it does not walk them.
        let mut root = el("body", None);
        for i in 0..100_000 {
            root.append_child(el("div", Some(&format!("n{i}"))));
        }
        let doc = Document::new(root);
        let started = std::time::Instant::now();
        for i in (0..100_000).step_by(97) {
            assert!(doc.find_by_id(&format!("n{i}")).is_some());
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "{:?}", started.elapsed());
    }

    #[test]
    fn a_deep_document_indexes_without_recursion() {
        let mut node = el("div", Some("leaf"));
        for _ in 0..100_000 {
            let mut parent = el("div", None);
            parent.append_child(node);
            node = parent;
        }
        let doc = Document::new(node);
        assert!(doc.find_by_id("leaf").is_some());
    }

    // ---- Milestone 39: the full mutation API (ADR 021 §7) ----

    fn with_id(tag: &str, id: &str) -> Element {
        let mut e = Element::new(tag);
        e.set_attr("id", id);
        e
    }

    /// A body with `n` `<p id=pK>` children.
    fn rows(n: usize) -> Document {
        let mut body = Element::new("body");
        for i in 0..n {
            body.append_child(with_id("p", &format!("p{i}")));
        }
        Document::new(body)
    }

    fn ids(doc: &Document) -> Vec<String> {
        doc.root().element_children().map(|e| e.attr("id").unwrap_or("").to_string()).collect()
    }

    #[test]
    fn insert_before_places_a_fragment_and_records_the_parent() {
        let mut doc = rows(3);
        let body = doc.root().node_id();
        let p1 = doc.node_id_of("p1").unwrap();
        doc.take_mutations();
        doc.insert_before(body, with_id("div", "new").into(), Some(p1)).unwrap();
        assert_eq!(ids(&doc), ["p0", "new", "p1", "p2"]);
        doc.insert_before(body, with_id("div", "last").into(), None).unwrap();
        assert_eq!(ids(&doc), ["p0", "new", "p1", "p2", "last"]);
        let records = doc.take_mutations().records;
        assert_eq!(records.iter().filter(|r| *r == &MutationRecord { target: body, kind: MutationKind::ChildList }).count(), 2);
        assert_eq!(records.iter().filter(|r| r.kind == MutationKind::Inserted).count(), 2, "each inserted node is recorded");
        assert_eq!(doc.find_by_id("new").map(|e| e.tag()), Some("div"));
        assert!(doc.index_is_consistent());
        // A reference that is not a child: refused, the fragment given back.
        let refused = doc.insert_before(doc.node_id_of("p0").unwrap(), with_id("i", "x").into(), Some(p1));
        assert!(matches!(refused, Err(Fragment::Element(e)) if e.attr("id") == Some("x")));
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn a_removed_subtree_keeps_its_ids_and_can_be_inserted_again() {
        let mut doc = rows(3);
        let body = doc.root().node_id();
        let p1 = doc.node_id_of("p1").unwrap();
        doc.set_text_content(p1, "hello");
        let removed = doc.remove_child(body, p1).expect("a child");
        assert_eq!(ids(&doc), ["p0", "p2"]);
        assert!(!doc.contains(p1) && doc.find_by_id("p1").is_none());
        assert_eq!(removed.id(), p1, "the same node, by identity");
        assert!(doc.index_is_consistent());
        doc.insert_before(body, removed, None).unwrap();
        assert_eq!(ids(&doc), ["p0", "p2", "p1"]);
        assert_eq!(doc.find_by_node_id(p1).map(|e| e.text_content()), Some("hello".to_string()));
        assert!(doc.remove_child(body, doc.root().node_id()).is_none(), "not a child");
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn replace_child_swaps_in_place_and_returns_the_old_node() {
        let mut doc = rows(3);
        let body = doc.root().node_id();
        let p1 = doc.node_id_of("p1").unwrap();
        let old = doc.replace_child(body, with_id("section", "s").into(), p1).unwrap();
        assert_eq!(old.id(), p1);
        assert_eq!(ids(&doc), ["p0", "s", "p2"]);
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn moving_an_attached_node_relinks_it_and_refuses_cycles() {
        let mut doc = rows(3);
        let body = doc.root().node_id();
        let (p0, p2) = (doc.node_id_of("p0").unwrap(), doc.node_id_of("p2").unwrap());
        assert!(doc.move_before(body, p2, Some(p0)));
        assert_eq!(ids(&doc), ["p2", "p0", "p1"]);
        // Into a sibling, then refuse to move an ancestor under its descendant.
        assert!(doc.move_before(p0, p2, None));
        assert_eq!(ids(&doc), ["p0", "p1"]);
        assert_eq!(doc.parent_id(p2), Some(p0));
        assert!(!doc.move_before(p2, p0, None), "p0 contains p2");
        assert!(!doc.move_before(p0, body, None), "the root never moves");
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn clone_node_gives_fresh_ids_and_shallow_clones_have_no_children() {
        let mut doc = rows(1);
        let p0 = doc.node_id_of("p0").unwrap();
        doc.set_text_content(p0, "text");
        let Some(Fragment::Element(deep)) = doc.clone_node(p0, true) else { panic!("an element") };
        assert_ne!(deep.node_id, p0);
        assert_eq!(deep.text_content(), "text");
        assert_eq!(deep.attr("id"), Some("p0"));
        let Some(Fragment::Element(shallow)) = doc.clone_node(p0, false) else { panic!("an element") };
        assert!(shallow.children.is_empty() && shallow.node_id != deep.node_id);
        // The clone can go in beside its original: different nodes.
        doc.insert_before(doc.root().node_id(), Fragment::Element(deep), None).unwrap();
        assert_eq!(doc.element_count(), 3);
        assert!(doc.index_is_consistent());
    }

    #[test]
    fn text_nodes_have_identity_in_the_document() {
        let mut doc = rows(1);
        let body = doc.root().node_id();
        let text = Fragment::text("hello");
        let id = text.id();
        doc.insert_before(body, text, None).unwrap();
        assert!(doc.contains_node(id) && !doc.contains(id), "a node, not an element");
        assert_eq!(doc.text_of(id), Some("hello"));
        assert_eq!(doc.parent_node_id(id), Some(body));
        assert_eq!(doc.root().text_content(), "hello");
        let removed = doc.remove_child(body, id).unwrap();
        assert_eq!(removed, Fragment::Text { id, text: "hello".into() });
        assert!(!doc.contains_node(id));
        assert!(doc.index_is_consistent());
    }

    /// The acceptance criterion: mutating a node costs O(1) link updates,
    /// whatever the document's size.
    #[test]
    fn mutations_cost_constant_link_updates_on_a_large_document() {
        let mut doc = rows(100_000);
        let body = doc.root().node_id();
        let middle = doc.node_id_of("p50000").unwrap();
        let last = doc.node_id_of("p99999").unwrap();
        let cost = |doc: &mut Document, f: &mut dyn FnMut(&mut Document)| {
            let before = doc.link_updates();
            f(doc);
            doc.link_updates() - before
        };
        assert_eq!(cost(&mut doc, &mut |d| d.insert_before(body, Element::new("hr").into(), Some(middle)).unwrap()), 1);
        assert_eq!(cost(&mut doc, &mut |d| assert!(d.move_before(body, last, Some(middle)))), 2);
        assert_eq!(cost(&mut doc, &mut |d| drop(d.remove_child(body, middle).unwrap())), 1);
        let first = doc.node_id_of("p0").unwrap();
        assert_eq!(cost(&mut doc, &mut |d| drop(d.replace_child(body, Element::new("hr").into(), first).unwrap())), 2);
        assert!(doc.index_is_consistent());
    }

    /// Random mutation sequences keep the arena and its indexes exactly
    /// consistent, and every freed slot is reused.
    #[test]
    fn random_mutations_keep_the_arena_consistent() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut below = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let mut doc = rows(20);
        for step in 0..3_000 {
            let all: Vec<NodeId> = doc.root().descendants_and_self().map(|e| e.node_id()).collect();
            let pick = |i: usize| all[i % all.len()];
            let (a, b) = (pick(below(1000)), pick(below(1000)));
            match below(7) {
                0 => {
                    let _ = doc.insert_before(a, with_id("i", &format!("n{step}")).into(), None);
                }
                1 => {
                    let reference = doc.root().find_by_node_id(a).and_then(|e| e.element_children().next()).map(|e| e.node_id());
                    let _ = doc.insert_before(a, Fragment::text("t"), reference);
                }
                2 => {
                    if let Some(parent) = doc.parent_id(b) {
                        let _ = doc.remove_child(parent, b);
                    }
                }
                3 => {
                    let _ = doc.move_before(a, b, None);
                }
                4 => {
                    if let (Some(parent), Some(Fragment::Element(clone))) = (doc.parent_id(b), doc.clone_node(a, true)) {
                        let _ = doc.replace_child(parent, Fragment::Element(clone), b);
                    }
                }
                5 => {
                    doc.set_text_content(a, "x");
                }
                _ => {
                    doc.set_attribute(a, "id", format!("r{}", below(5)));
                }
            }
            assert!(doc.index_is_consistent(), "inconsistent after step {step}");
        }
    }

    /// Milestone 40b (ADR 021 §5): names are interned per document. A page
    /// inventing a million attribute names grows only its own document's
    /// table — a second document's is untouched, and well-known names add
    /// nothing — and a document's names read back as written.
    #[test]
    fn invented_names_grow_only_their_own_documents_table() {
        let (elements, per_element) = (100_000, 10);
        let mut root = Element::new("html");
        for i in 0..elements {
            let mut e = Element::new(if i % 2 == 0 { "div" } else { "x-widget" });
            for k in 0..per_element {
                e.attributes.push((Name::from(format!("data-n{}", i * per_element + k)), String::new()));
            }
            e.set_attr("class", "c");
            root.append_child(e);
        }
        let big = Document::new(root);
        // A million attribute names and one custom tag; `class` and `div`
        // are well-known.
        assert_eq!(big.invented_names(), elements * per_element + 1);
        let other = Document::new(el("div", Some("a")));
        assert_eq!(other.invented_names(), 0, "another document's table is its own");
        let last = big.root().element_children().last().expect("children");
        assert_eq!(last.tag(), "x-widget");
        assert_eq!(last.attributes().next(), Some(("data-n999990", "")));
        assert_eq!(last.attr("data-n999999"), Some(""));
        assert_eq!(last.attr_atom(atoms::CLASS), Some("c"));
        drop(big);
        assert_eq!(Document::new(el("div", None)).invented_names(), 0, "nothing outlives a document");
    }

    /// Names a script sets are interned once, read back as text, leave the
    /// document as text (a fragment has no table) and are found again when
    /// the fragment comes back.
    #[test]
    fn script_names_are_interned_and_survive_removal() {
        let mut doc = rows(2);
        let p0 = doc.node_id_of("p0").unwrap();
        for _ in 0..3 {
            assert!(doc.set_attribute(p0, "data-x", "1"));
        }
        assert!(doc.set_attribute(p0, "title", "t"));
        assert_eq!(doc.invented_names(), 1, "data-x once; title is well-known");
        assert_eq!(doc.find_by_node_id(p0).unwrap().attr("data-x"), Some("1"));
        let root = doc.root().node_id();
        let Some(Fragment::Element(removed)) = doc.remove_child(root, p0) else { panic!("removed") };
        assert_eq!(removed.attr("data-x"), Some("1"));
        assert!(removed.attributes.iter().any(|(k, _)| k == "data-x" && k.atom().is_none()), "detached names carry their text");
        doc.insert_before(root, Fragment::Element(removed), None).unwrap();
        assert_eq!(doc.find_by_node_id(p0).unwrap().attr("data-x"), Some("1"));
        assert_eq!(doc.invented_names(), 1);
        assert!(doc.remove_attribute(p0, "data-x"));
        assert!(doc.remove_attribute(p0, "never-seen"), "an unknown name is absent, not an error");
        assert_eq!(doc.invented_names(), 1, "an unknown name is not interned by removing it");
        assert!(doc.index_is_consistent());
    }
}
