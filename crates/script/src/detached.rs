//! Nodes that are not (yet) in the document — the "detached forest"
//! (Milestone 18): elements from `document.createElement`, `cloneNode` or
//! `removeChild`, with their subtrees, and lone text nodes
//! (`document.createTextNode`, a removed text child).
//!
//! Every lookup is O(1) for a detached root and O(depth) for a node nested
//! inside a detached subtree: each nested node's path from its root is
//! indexed. (The first version scanned every detached subtree on every
//! call, so a script that created and configured N elements did O(N²)
//! work.)
//!
//! A detached subtree that no script wrapper can reach any more is freed:
//! a runtime opens a handle on a node when it makes the node's wrapper and
//! closes it when the wrapper is collected (Milestone 40, ADR 023 §4), and
//! a subtree none of whose nodes has an open handle can never be attached,
//! so it goes. (It used to stay for the life of the page — a script
//! creating elements in a loop grew memory without bound.)

use dom::{Element, Node};
use std::cell::Cell;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct DetachedForest {
    /// Detached subtree roots, by node id.
    roots: HashMap<u64, Element>,
    /// Every detached node nested below a root: its root, and its path of
    /// child indices from that root.
    nested: HashMap<u64, (u64, Vec<u32>)>,
    /// Lone detached text nodes.
    texts: HashMap<u64, String>,
    /// Open script handles per node (attached or not).
    handles: HashMap<u64, usize>,
    /// Elements stepped through, ever (tests bound the work a script's
    /// detached-node operations cost).
    visits: Cell<u64>,
}

impl DetachedForest {
    /// Adds a new detached root.
    pub(crate) fn create(&mut self, element: Element) {
        self.roots.insert(element.node_id, element);
    }

    /// Adds a lone text node.
    pub(crate) fn create_text(&mut self, id: u64, text: String) {
        self.texts.insert(id, text);
    }

    /// A lone detached text node's text.
    pub(crate) fn text(&self, id: u64) -> Option<&str> {
        self.texts.get(&id).map(String::as_str)
    }

    pub(crate) fn set_text(&mut self, id: u64, text: &str) -> bool {
        match self.texts.get_mut(&id) {
            Some(t) => {
                *t = text.to_string();
                true
            }
            None => false,
        }
    }

    /// Removes a lone text node, to insert it.
    pub(crate) fn take_text(&mut self, id: u64) -> Option<String> {
        self.texts.remove(&id)
    }

    /// Whether `node` is here: a root, nested below one, or a lone text node.
    pub(crate) fn contains(&self, node: u64) -> bool {
        self.roots.contains_key(&node) || self.nested.contains_key(&node) || self.texts.contains_key(&node)
    }

    /// A script wrapper for `node` exists.
    pub(crate) fn open(&mut self, node: u64) {
        *self.handles.entry(node).or_insert(0) += 1;
    }

    /// How many detached roots are held (tests, diagnostics).
    pub(crate) fn root_count(&self) -> usize {
        self.roots.len()
    }

    /// Elements stepped through so far.
    pub(crate) fn visits(&self) -> u64 {
        self.visits.get()
    }

    /// How many detached nodes are held, roots, nested and text (tests).
    pub(crate) fn node_count(&self) -> usize {
        self.roots.len() + self.nested.len() + self.texts.len()
    }

    pub(crate) fn with<R>(&self, node: u64, f: impl FnOnce(&Element) -> R) -> Option<R> {
        if let Some(root) = self.roots.get(&node) {
            return Some(f(root));
        }
        let (root, path) = self.nested.get(&node)?;
        let el = element_at(self.roots.get(root)?, path, &self.visits)?;
        (el.node_id == node).then(|| f(el))
    }

    pub(crate) fn with_mut<R>(&mut self, node: u64, f: impl FnOnce(&mut Element) -> R) -> Option<R> {
        let (root, path) = if self.roots.contains_key(&node) {
            (node, Vec::new())
        } else {
            self.nested.get(&node)?.clone()
        };
        let el = element_at_mut(self.roots.get_mut(&root)?, &path, &self.visits)?;
        if el.node_id != node {
            return None;
        }
        // The only way `f` changes children is by replacing them (a
        // `textContent` write): a new vector, or a new length.
        let before = (el.children.as_ptr(), el.children.len());
        let result = f(el);
        if (el.children.as_ptr(), el.children.len()) != before {
            self.reindex_root(root);
        }
        Some(result)
    }

    /// Removes `node` (and its subtree) from the forest, to attach it
    /// somewhere. `None` if it is not detached.
    pub(crate) fn take(&mut self, node: u64) -> Option<Element> {
        if let Some(el) = self.roots.remove(&node) {
            self.unindex_below(&el);
            return Some(el);
        }
        let (root, path) = self.nested.get(&node)?.clone();
        let (last, parent_path) = path.split_last()?;
        let parent = element_at_mut(self.roots.get_mut(&root)?, parent_path, &self.visits)?;
        let taken = match parent.children.get(*last as usize) {
            Some(Node::Element(el)) if el.node_id == node => match parent.children.remove(*last as usize) {
                Node::Element(el) => el,
                Node::Text(_) => unreachable!("checked above"),
            },
            _ => return None,
        };
        self.nested.remove(&node);
        self.unindex_below(&taken);
        // Later siblings moved up by one: re-index the root's subtree.
        self.reindex_root(root);
        Some(taken)
    }

    /// Appends `child` to the detached node `parent`. Gives `child` back
    /// if `parent` is not detached.
    pub(crate) fn append(&mut self, parent: u64, child: Element) -> Result<(), Element> {
        let (root, mut path) = if self.roots.contains_key(&parent) {
            (parent, Vec::new())
        } else {
            match self.nested.get(&parent) {
                Some((root, path)) => (*root, path.clone()),
                None => return Err(child),
            }
        };
        let Some(parent_el) = self.roots.get_mut(&root).and_then(|r| element_at_mut(r, &path, &self.visits)) else { return Err(child) };
        path.push(parent_el.children.len() as u32);
        parent_el.children.push(Node::Element(child));
        let Some(Node::Element(added)) = parent_el.children.last() else { unreachable!("just pushed") };
        let mut entries = Vec::new();
        index_subtree(added, root, path, &mut entries, &self.visits);
        self.nested.extend(entries);
        Ok(())
    }

    /// Inserts `child` into the detached node `parent`, before its child
    /// element `before` (`None`: last). Gives `child` back if `parent` is
    /// not detached or `before` is not one of its child elements.
    pub(crate) fn insert_before(&mut self, parent: u64, child: Element, before: Option<u64>) -> Result<(), Element> {
        let Some(before) = before else { return self.append(parent, child) };
        let (root, path) = if self.roots.contains_key(&parent) {
            (parent, Vec::new())
        } else {
            match self.nested.get(&parent) {
                Some((root, path)) => (*root, path.clone()),
                None => return Err(child),
            }
        };
        let Some(parent_el) = self.roots.get_mut(&root).and_then(|r| element_at_mut(r, &path, &self.visits)) else { return Err(child) };
        let Some(index) = parent_el.children.iter().position(|c| matches!(c, Node::Element(e) if e.node_id == before)) else { return Err(child) };
        parent_el.children.insert(index, Node::Element(child));
        self.reindex_root(root);
        Ok(())
    }

    /// The detached element `node` is a child of, if it is nested.
    pub(crate) fn parent_of(&self, node: u64) -> Option<u64> {
        let (root, path) = self.nested.get(&node)?;
        let (_, parent_path) = path.split_last()?;
        element_at(self.roots.get(root)?, parent_path, &self.visits).map(|e| e.node_id)
    }

    /// Puts an element taken with [`Self::take`] back as a root (an attach
    /// that failed).
    pub(crate) fn restore(&mut self, element: Element) {
        let root = element.node_id;
        let mut entries = Vec::new();
        for (i, child) in element.children.iter().enumerate() {
            if let Node::Element(c) = child {
                index_subtree(c, root, vec![i as u32], &mut entries, &self.visits);
            }
        }
        self.nested.extend(entries);
        self.roots.insert(root, element);
    }

    /// The wrappers of `closed` were collected: frees the detached subtrees
    /// and text nodes no open handle can reach any more.
    pub(crate) fn close(&mut self, closed: impl IntoIterator<Item = u64>) {
        for node in closed {
            if let Some(n) = self.handles.get_mut(&node) {
                *n -= 1;
                if *n == 0 {
                    self.handles.remove(&node);
                }
            }
            if self.texts.contains_key(&node) {
                if !self.handles.contains_key(&node) {
                    self.texts.remove(&node);
                }
                continue;
            }
            let root = if self.roots.contains_key(&node) { Some(node) } else { self.nested.get(&node).map(|(r, _)| *r) };
            let Some(root) = root else { continue }; // attached, or already freed
            let reachable = self.roots.get(&root).is_some_and(|el| any_node(el, &|n| self.handles.contains_key(&n), &self.visits));
            if !reachable {
                if let Some(el) = self.roots.remove(&root) {
                    self.unindex_below(&el);
                }
            }
        }
    }

    fn unindex_below(&mut self, element: &Element) {
        let mut stack: Vec<&Element> = vec![element];
        while let Some(el) = stack.pop() {
            visit(&self.visits);
            for child in &el.children {
                if let Node::Element(c) = child {
                    self.nested.remove(&c.node_id);
                    stack.push(c);
                }
            }
        }
    }

    fn reindex_root(&mut self, root: u64) {
        self.nested.retain(|_, (r, _)| *r != root);
        let Some(el) = self.roots.get(&root) else { return };
        let mut entries = Vec::new();
        for (i, child) in el.children.iter().enumerate() {
            if let Node::Element(c) = child {
                index_subtree(c, root, vec![i as u32], &mut entries, &self.visits);
            }
        }
        self.nested.extend(entries);
    }
}

/// Records `element` (at `path` below `root`) and its subtree.
/// Iterative: a script can build an arbitrarily deep detached tree.
fn index_subtree(element: &Element, root: u64, path: Vec<u32>, out: &mut Vec<(u64, (u64, Vec<u32>))>, visits: &Cell<u64>) {
    let mut stack = vec![(element, path)];
    while let Some((el, path)) = stack.pop() {
        visit(visits);
        for (i, child) in el.children.iter().enumerate() {
            if let Node::Element(c) = child {
                let mut p = path.clone();
                p.push(i as u32);
                stack.push((c, p));
            }
        }
        out.push((el.node_id, (root, path)));
    }
}

fn any_node(element: &Element, pred: &dyn Fn(u64) -> bool, visits: &Cell<u64>) -> bool {
    let mut stack = vec![element];
    while let Some(el) = stack.pop() {
        visit(visits);
        if pred(el.node_id) {
            return true;
        }
        stack.extend(el.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        }));
    }
    false
}

#[inline]
fn visit(visits: &Cell<u64>) {
    visits.set(visits.get() + 1);
}

fn element_at<'e>(root: &'e Element, path: &[u32], visits: &Cell<u64>) -> Option<&'e Element> {
    let mut el = root;
    visit(visits);
    for &i in path {
        visit(visits);
        match el.children.get(i as usize)? {
            Node::Element(c) => el = c,
            Node::Text(_) => return None,
        }
    }
    Some(el)
}

fn element_at_mut<'e>(root: &'e mut Element, path: &[u32], visits: &Cell<u64>) -> Option<&'e mut Element> {
    let mut el = root;
    visit(visits);
    for &i in path {
        visit(visits);
        match el.children.get_mut(i as usize)? {
            Node::Element(c) => el = c,
            Node::Text(_) => return None,
        }
    }
    Some(el)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forest_with(n: usize) -> (DetachedForest, Vec<u64>) {
        let mut forest = DetachedForest::default();
        let mut ids = Vec::new();
        for _ in 0..n {
            let el = Element::new("div");
            ids.push(el.node_id);
            forest.open(el.node_id);
            forest.create(el);
        }
        (forest, ids)
    }

    #[test]
    fn nested_nodes_are_found_by_path_and_survive_moves() {
        let (mut forest, ids) = forest_with(4);
        let [a, b, c, d] = [ids[0], ids[1], ids[2], ids[3]];
        let taken = forest.take(b).unwrap();
        forest.append(a, taken).unwrap();
        let taken = forest.take(c).unwrap();
        forest.append(a, taken).unwrap();
        let taken = forest.take(d).unwrap();
        forest.append(c, taken).unwrap();
        forest.with_mut(d, |el| el.set_attr("id", "deep")).unwrap();
        assert_eq!(forest.with(d, |el| el.attr_atom(dom::atoms::ID).map(str::to_string)).flatten().as_deref(), Some("deep"));
        // Moving `b` out shifts `c` (and `d` under it) left by one.
        let b_el = forest.take(b).unwrap();
        assert_eq!(b_el.node_id, b);
        assert_eq!(forest.with(d, |el| el.node_id), Some(d), "paths re-indexed after a move");
        forest.restore(b_el);
        assert_eq!(forest.root_count(), 2);
        assert_eq!(forest.node_count(), 4);
    }

    #[test]
    fn text_content_replacing_children_drops_them_from_the_index() {
        let (mut forest, ids) = forest_with(2);
        let taken = forest.take(ids[1]).unwrap();
        forest.append(ids[0], taken).unwrap();
        forest.with_mut(ids[0], |el| el.set_text_content("x")).unwrap();
        assert!(forest.with(ids[1], |_| ()).is_none());
        assert_eq!(forest.node_count(), 1);
    }

    #[test]
    fn a_subtree_is_freed_when_no_handle_can_reach_it() {
        let (mut forest, ids) = forest_with(3);
        let taken = forest.take(ids[1]).unwrap();
        forest.append(ids[0], taken).unwrap();
        // Close the root's handle: the child's handle still reaches the tree.
        forest.close([ids[0]]);
        assert!(forest.with(ids[0], |_| ()).is_some());
        // Close the child's handle too: nothing reaches it.
        forest.close([ids[1]]);
        assert!(forest.with(ids[0], |_| ()).is_none());
        assert_eq!(forest.node_count(), 1, "only the third, still-held element is left");
    }
}
