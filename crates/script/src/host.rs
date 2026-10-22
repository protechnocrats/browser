//! The host side of the bindings (ADR 023 §2): [`HostState`] implements the
//! traits `build.rs` generates from the IDL. Nodes are named by their
//! `dom::NodeId` wherever they are — in the document or in the detached
//! forest — so one wrapper names one node for its whole life (ADR 023 §4).
//! Every DOM change goes through `dom::Document`'s methods (ADR 021).

use crate::bindings::*;
use crate::detached::DetachedForest;
use crate::{net, CallbackId, HostError, Interface, ObjectRef, PromiseId, PromiseValue, Value};
use dom::{Document, Element, ElementRef, Fragment, NodeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The host state one runtime calls into.
pub type HostRef = Rc<RefCell<HostState>>;

/// The document object (there is one per environment).
pub const DOCUMENT: ObjectRef = ObjectRef { interface: Interface::Document, key: 0 };
/// The global object: `this` for `[Global]` members.
pub const WINDOW: ObjectRef = ObjectRef { interface: Interface::Window, key: 0 };
const LOCAL_STORAGE: ObjectRef = ObjectRef { interface: Interface::Storage, key: 0 };
const SESSION_STORAGE: ObjectRef = ObjectRef { interface: Interface::Storage, key: 1 };

/// The properties `CSSStyleDeclaration` exposes, in IDL order, with their
/// CSS names.
const STYLE_PROPERTIES: [&str; 7] = ["color", "background-color", "width", "height", "font-size", "text-align", "transform"];

/// Held by a runtime's wrapper for `object`; dropped when the runtime
/// collects the wrapper, which tells the host (ADR 023 §4). It never borrows
/// the host: a collection can run in the middle of a binding.
pub struct WrapperGuard {
    object: ObjectRef,
    closed: Rc<RefCell<Vec<ObjectRef>>>,
}

impl Drop for WrapperGuard {
    fn drop(&mut self) {
        if let Ok(mut closed) = self.closed.try_borrow_mut() {
            closed.push(self.object);
        }
    }
}

/// A host object that is not a node: what its wrapper reaches.
pub(crate) enum HostObject {
    Event(crate::events::EventData),
    Response(net::ResponseData),
    Headers(Vec<(String, String)>),
    Xhr(net::XhrData),
}

pub(crate) struct Slot {
    pub(crate) object: HostObject,
    /// Live wrappers.
    wrappers: u32,
    /// Host code using it (an event being dispatched, an XHR in flight).
    pins: u32,
}

/// Where `localStorage`/`sessionStorage` entries live (Milestone 36).
pub(crate) enum StorageSource {
    /// `localStorage` with a capability: the area of this document's
    /// storage key, for as long as the grant is not revoked.
    Granted(capability::OriginStorage),
    /// Memory that belongs to this page alone (`sessionStorage`): no shared
    /// authority, so no capability.
    Own(storage::StorageArea),
    /// No capability, or one for another key: every method throws.
    Denied(capability::Denial),
}

impl StorageSource {
    fn area(&self) -> Result<&storage::StorageArea, HostError> {
        match self {
            StorageSource::Granted(granted) => granted.area().map_err(|e| HostError::error(e.to_string())),
            StorageSource::Own(area) => Ok(area),
            StorageSource::Denied(denial) => {
                Err(HostError::error(capability::CapabilityError::report(capability::CapabilityKind::Storage, *denial).to_string()))
            }
        }
    }
}

pub struct HostState {
    pub(crate) document: Rc<RefCell<Document>>,
    /// Has script changed the document since the engine last asked?
    pub(crate) dirty: bool,
    pub(crate) forest: DetachedForest,
    closed: Rc<RefCell<Vec<ObjectRef>>>,
    objects: HashMap<u64, Slot>,
    next_object: u64,
    /// `target -> event type -> (callback, capture)`. Nodes are keyed as
    /// `Interface::Node`, whatever their concrete interface.
    pub(crate) listeners: HashMap<ObjectRef, HashMap<String, Vec<(CallbackId, bool)>>>,
    pub(crate) animation_frames: Vec<CallbackId>,
    next_animation_frame: u32,
    /// Callbacks the host dropped, for the runtime to forget.
    released: Vec<CallbackId>,
    local_storage: StorageSource,
    session_storage: StorageSource,
    pub(crate) net: net::NetState,
    next_promise: u64,
    /// Wrappers runtimes have made, ever (tests bound it).
    wrappers_opened: u64,
}

impl HostState {
    pub(crate) fn new(document: Rc<RefCell<Document>>, local_storage: StorageSource, net: net::NetState) -> HostState {
        HostState {
            document,
            dirty: false,
            forest: DetachedForest::default(),
            closed: Rc::default(),
            objects: HashMap::new(),
            next_object: 1,
            listeners: HashMap::new(),
            animation_frames: Vec::new(),
            next_animation_frame: 0,
            released: Vec::new(),
            local_storage,
            session_storage: StorageSource::Own(storage::StorageArea::private()),
            net,
            next_promise: 1,
            wrappers_opened: 0,
        }
    }

    /// A runtime made a wrapper for `object`; it keeps the guard as long as
    /// the wrapper lives.
    pub fn open_wrapper(&mut self, object: ObjectRef) -> WrapperGuard {
        self.wrappers_opened += 1;
        if object.interface.inherits(Interface::Node) {
            self.forest.open(object.key);
        } else if let Some(slot) = self.objects.get_mut(&object.key) {
            slot.wrappers += 1;
        }
        WrapperGuard { object, closed: Rc::clone(&self.closed) }
    }

    /// Frees what collected wrappers left unreachable: detached nodes, host
    /// objects no wrapper and no host code holds. Called before every
    /// binding call and at every engine entry point — never between a
    /// binding's return and its result's wrapping, which the runtime does at
    /// once.
    pub fn collect_closed(&mut self) {
        let closed = std::mem::take(&mut *self.closed.borrow_mut());
        if closed.is_empty() {
            return;
        }
        let mut nodes = Vec::new();
        for object in closed {
            if object.interface.inherits(Interface::Node) {
                nodes.push(object.key);
            } else if let Some(slot) = self.objects.get_mut(&object.key) {
                slot.wrappers = slot.wrappers.saturating_sub(1);
                if slot.wrappers == 0 && slot.pins == 0 {
                    self.free_object(object.key);
                }
            }
        }
        self.forest.close(nodes);
    }

    fn free_object(&mut self, key: u64) {
        if let Some(slot) = self.objects.remove(&key) {
            if let HostObject::Xhr(xhr) = slot.object {
                self.released.extend(xhr.handlers.into_iter().flatten());
            }
        }
    }

    /// Callbacks the host dropped since the last call.
    pub fn take_released(&mut self) -> Vec<CallbackId> {
        std::mem::take(&mut self.released)
    }

    pub(crate) fn release(&mut self, callback: Option<CallbackId>) {
        self.released.extend(callback);
    }

    pub(crate) fn new_object(&mut self, object: HostObject) -> u64 {
        let key = self.next_object;
        self.next_object += 1;
        self.objects.insert(key, Slot { object, wrappers: 0, pins: 0 });
        key
    }

    /// Keeps a host object alive while host code uses it, wrapped or not.
    pub(crate) fn pin(&mut self, key: u64) {
        if let Some(slot) = self.objects.get_mut(&key) {
            slot.pins += 1;
        }
    }

    pub(crate) fn unpin(&mut self, key: u64) {
        if let Some(slot) = self.objects.get_mut(&key) {
            slot.pins = slot.pins.saturating_sub(1);
            if slot.pins == 0 && slot.wrappers == 0 {
                self.free_object(key);
            }
        }
    }

    pub(crate) fn object(&self, key: u64) -> Option<&HostObject> {
        self.objects.get(&key).map(|s| &s.object)
    }

    pub(crate) fn object_mut(&mut self, key: u64) -> Option<&mut HostObject> {
        self.objects.get_mut(&key).map(|s| &mut s.object)
    }

    /// `localStorage` granted again (`ScriptEnvironment::grant`).
    pub(crate) fn set_local_storage(&mut self, granted: capability::OriginStorage) {
        self.local_storage = StorageSource::Granted(granted);
    }

    /// Whether the node `id` is in the document. A runtime keeps the
    /// wrappers of such nodes (and their expando properties) alive: the
    /// document can always reach the node again (M40's gap; Blink does the
    /// same for nodes in a document).
    pub fn is_in_document(&self, id: NodeId) -> bool {
        self.attached(id)
    }

    /// The element `id` and its descendant elements, in the document or
    /// detached — for a runtime to keep their wrappers once it is inserted,
    /// and let them go once it is removed.
    pub fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let ids = |el: dom::ElementRef<'_>| el.descendants_and_self().map(|e| e.node_id()).collect();
        let doc = self.document.borrow();
        match doc.find_by_node_id(id) {
            Some(el) => ids(el),
            None => self.forest.with(id, |el| ids(dom::ElementRef::from(el))).unwrap_or_default(),
        }
    }

    /// The document's link updates so far: a runtime re-checks which kept
    /// wrappers' nodes are still in the document only after this has grown
    /// by as many as it keeps (amortised O(1) per tree change).
    pub fn document_link_updates(&self) -> u64 {
        self.document.borrow().link_updates()
    }

    /// How many wrappers runtimes have made for this host, ever (tests: one
    /// per node, however often script asks for it).
    pub fn wrappers_opened(&self) -> u64 {
        self.wrappers_opened
    }

    /// How many host objects (not nodes) are held (tests).
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// How many detached roots are held (tests, diagnostics).
    pub fn detached_root_count(&self) -> usize {
        self.forest.root_count()
    }

    /// How many detached nodes are held (tests).
    pub fn detached_node_count(&self) -> usize {
        self.forest.node_count()
    }

    /// Elements the detached forest has stepped through (tests bound it).
    pub fn detached_visits(&self) -> u64 {
        self.forest.visits()
    }

    pub(crate) fn new_promise(&mut self) -> PromiseId {
        let id = PromiseId(self.next_promise);
        self.next_promise += 1;
        id
    }

    // ---- Nodes ----

    /// The wrapper reference of node `id`: an element or a text node, in the
    /// document or detached. `None` for a node that no longer exists.
    pub fn node_ref(&self, id: NodeId) -> Option<ObjectRef> {
        let doc = self.document.borrow();
        if doc.find_by_node_id(id).is_some() {
            return Some(ObjectRef::new(Interface::Element, id));
        }
        if doc.text_of(id).is_some() || self.forest.text(id).is_some() {
            return Some(ObjectRef::new(Interface::Text, id));
        }
        self.forest.with(id, |_| ObjectRef::new(Interface::Element, id))
    }

    /// Reads the element `id`, wherever it is.
    fn with_element<R>(&self, id: NodeId, f: impl FnOnce(ElementRef<'_>) -> R) -> Option<R> {
        let doc = self.document.borrow();
        match doc.find_by_node_id(id) {
            Some(el) => Some(f(el)),
            None => self.forest.with(id, |el| f(el.into())),
        }
    }

    fn attached(&self, id: NodeId) -> bool {
        self.document.borrow().contains_node(id)
    }

    fn set_attribute_on(&mut self, id: NodeId, name: &str, value: &str) {
        if self.attached(id) {
            if self.document.borrow_mut().set_attribute(id, name, value) {
                self.dirty = true;
            }
        } else {
            self.forest.with_mut(id, |el| el.set_attr(name, value));
        }
    }

    fn remove_attribute_on(&mut self, id: NodeId, name: &str) {
        if self.attached(id) {
            if self.document.borrow_mut().remove_attribute(id, name) {
                self.dirty = true;
            }
        } else {
            self.forest.with_mut(id, |el| el.remove_attr(name));
        }
    }

    /// Takes the node `node` out of wherever it is, as a fragment to insert
    /// elsewhere. `None` if it no longer exists or cannot move (the root).
    fn take_node(&mut self, node: NodeId) -> Option<Fragment> {
        if self.attached(node) {
            let mut doc = self.document.borrow_mut();
            let parent = doc.parent_node_id(node)?;
            let fragment = doc.remove_child(parent, node)?;
            self.dirty = true;
            return Some(fragment);
        }
        if let Some(text) = self.forest.take_text(node) {
            return Some(Fragment::Text { id: node, text });
        }
        self.forest.take(node).map(Fragment::Element)
    }

    /// Puts a fragment taken from its place back, as a detached node.
    fn detach(&mut self, fragment: Fragment) {
        match fragment {
            Fragment::Element(el) => self.forest.create(el),
            Fragment::Text { id, text } => self.forest.create_text(id, text),
        }
    }

    /// `parent.insertBefore(node, reference)`, wherever each of them is.
    fn insert(&mut self, parent: ObjectRef, node: ObjectRef, reference: Option<ObjectRef>) -> Result<(), HostError> {
        if parent.interface != Interface::Element {
            return Err(HostError::dom("HierarchyRequestError", "only an element can have children"));
        }
        let (parent_id, node_id, reference_id) = (parent.key, node.key, reference.map(|r| r.key));
        if reference_id == Some(node_id) {
            return Ok(()); // inserting a node before itself changes nothing
        }
        if self.attached(parent_id) {
            if reference_id.is_some_and(|r| self.document.borrow().parent_node_id(r) != Some(parent_id)) {
                return Err(HostError::dom("NotFoundError", "the reference node is not a child of this node"));
            }
            if self.attached(node_id) {
                if !self.document.borrow_mut().move_before(parent_id, node_id, reference_id) {
                    return Err(HostError::dom("HierarchyRequestError", "the node is this node or one of its ancestors"));
                }
                self.dirty = true;
                return Ok(());
            }
            let fragment = self.take_node(node_id).ok_or_else(gone)?;
            let inserted = self.document.borrow_mut().insert_before(parent_id, fragment, reference_id);
            return match inserted {
                Ok(()) => {
                    self.dirty = true;
                    Ok(())
                }
                Err(fragment) => {
                    self.detach(fragment);
                    Err(HostError::dom("HierarchyRequestError", "the node cannot be inserted here"))
                }
            };
        }
        if !self.forest.contains(parent_id) {
            return Err(gone());
        }
        // `parent` is detached: so is where `node` goes.
        if self.forest.with(node_id, |n| n.find_by_node_id(parent_id).is_some()).unwrap_or(false) {
            return Err(HostError::dom("HierarchyRequestError", "the node is this node or one of its ancestors"));
        }
        if reference_id.is_some_and(|r| self.forest.parent_of(r) != Some(parent_id)) {
            return Err(HostError::dom("NotFoundError", "the reference node is not a child of this node"));
        }
        let element = match self.take_node(node_id).ok_or_else(gone)? {
            Fragment::Element(el) => el,
            // Text inside a detached subtree has no identity (ADR 021 §4).
            Fragment::Text { text, .. } => {
                let inserted = self.forest.with_mut(parent_id, |el| el.children.push(dom::Node::Text(text)));
                return inserted.ok_or_else(gone);
            }
        };
        match self.forest.insert_before(parent_id, element, reference_id) {
            Ok(()) => Ok(()),
            Err(element) => {
                self.forest.restore(element);
                Err(HostError::dom("HierarchyRequestError", "the node cannot be inserted here"))
            }
        }
    }

    /// `parent.removeChild(child)`.
    fn remove(&mut self, parent: NodeId, child: NodeId) -> Result<(), HostError> {
        if self.attached(parent) {
            let removed = self.document.borrow_mut().remove_child(parent, child);
            let fragment = removed.ok_or_else(|| HostError::dom("NotFoundError", "the node is not a child of this node"))?;
            self.dirty = true;
            self.detach(fragment);
            return Ok(());
        }
        if self.forest.parent_of(child) != Some(parent) {
            return Err(HostError::dom("NotFoundError", "the node is not a child of this node"));
        }
        let element = self.forest.take(child).ok_or_else(gone)?;
        self.forest.create(element);
        Ok(())
    }
}

/// A node a wrapper names that no longer exists (a text node inside a
/// removed subtree, a subtree replaced by `textContent`).
fn gone() -> HostError {
    HostError::dom("NotFoundError", "the node no longer exists")
}

impl EventTargetImpl for HostState {
    fn add_event_listener(&mut self, this: ObjectRef, r#type: String, callback: Option<CallbackId>, capture: bool) -> Result<(), HostError> {
        if let Some(callback) = callback {
            self.listeners.entry(crate::events::listener_key(this)).or_default().entry(r#type).or_default().push((callback, capture));
        }
        Ok(())
    }
}

impl NodeImpl for HostState {
    fn node_type(&mut self, this: ObjectRef) -> Result<u16, HostError> {
        Ok(if this.interface == Interface::Text { 3 } else { 1 })
    }

    fn text_content(&mut self, this: ObjectRef) -> Result<Option<String>, HostError> {
        if this.interface == Interface::Text {
            let doc = self.document.borrow();
            let text = doc.text_of(this.key).or_else(|| self.forest.text(this.key));
            return Ok(Some(text.unwrap_or_default().to_string()));
        }
        Ok(Some(self.with_element(this.key, |el| el.text_content()).unwrap_or_default()))
    }

    fn set_text_content(&mut self, this: ObjectRef, value: Option<String>) -> Result<(), HostError> {
        let text = value.unwrap_or_default();
        if this.interface == Interface::Text {
            if self.document.borrow_mut().set_text(this.key, &text) {
                self.dirty = true;
            } else {
                self.forest.set_text(this.key, &text);
            }
            return Ok(());
        }
        if self.attached(this.key) {
            if self.document.borrow_mut().set_text_content(this.key, &text) {
                self.dirty = true;
            }
        } else {
            self.forest.with_mut(this.key, |el| el.set_text_content(&text));
        }
        Ok(())
    }

    fn append_child(&mut self, this: ObjectRef, node: ObjectRef) -> Result<ObjectRef, HostError> {
        self.insert(this, node, None)?;
        Ok(node)
    }

    fn insert_before(&mut self, this: ObjectRef, node: ObjectRef, child: Option<ObjectRef>) -> Result<ObjectRef, HostError> {
        self.insert(this, node, child)?;
        Ok(node)
    }

    fn remove_child(&mut self, this: ObjectRef, child: ObjectRef) -> Result<ObjectRef, HostError> {
        self.remove(this.key, child.key)?;
        Ok(child)
    }

    fn replace_child(&mut self, this: ObjectRef, node: ObjectRef, child: ObjectRef) -> Result<ObjectRef, HostError> {
        if node == child {
            return Ok(child);
        }
        let attached = self.attached(this.key);
        let is_child = if attached { self.document.borrow().parent_node_id(child.key) == Some(this.key) } else { self.forest.parent_of(child.key) == Some(this.key) };
        if !is_child {
            return Err(HostError::dom("NotFoundError", "the node to be replaced is not a child of this node"));
        }
        self.insert(this, node, Some(child))?;
        self.remove(this.key, child.key)?;
        Ok(child)
    }

    fn clone_node(&mut self, this: ObjectRef, deep: bool) -> Result<ObjectRef, HostError> {
        if this.interface == Interface::Text {
            let text = self.text_content(this)?.unwrap_or_default();
            let Fragment::Text { id, text } = Fragment::text(text) else { unreachable!("a text fragment") };
            self.forest.create_text(id, text);
            return Ok(ObjectRef::new(Interface::Text, id));
        }
        let clone = self.with_element(this.key, |el| el.to_element().clone_node(deep)).ok_or_else(gone)?;
        let id = clone.node_id;
        self.forest.create(clone);
        Ok(ObjectRef::new(Interface::Element, id))
    }
}

impl DocumentImpl for HostState {
    fn get_element_by_id(&mut self, _this: ObjectRef, element_id: String) -> Result<Option<ObjectRef>, HostError> {
        Ok(self.document.borrow().node_id_of(&element_id).map(|id| ObjectRef::new(Interface::Element, id)))
    }

    fn query_selector(&mut self, _this: ObjectRef, selectors: String) -> Result<Option<ObjectRef>, HostError> {
        let doc = self.document.borrow();
        Ok(doc.root().query_selector(&selectors).map(|el| ObjectRef::new(Interface::Element, el.node_id())))
    }

    fn create_element(&mut self, _this: ObjectRef, local_name: String) -> Result<ObjectRef, HostError> {
        let element = Element::new(local_name);
        let id = element.node_id;
        self.forest.create(element);
        Ok(ObjectRef::new(Interface::Element, id))
    }

    fn create_text_node(&mut self, _this: ObjectRef, data: String) -> Result<ObjectRef, HostError> {
        let Fragment::Text { id, text } = Fragment::text(data) else { unreachable!("a text fragment") };
        self.forest.create_text(id, text);
        Ok(ObjectRef::new(Interface::Text, id))
    }
}

impl ElementImpl for HostState {
    fn tag_name(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.with_element(this.key, |el| el.tag().to_ascii_uppercase()).unwrap_or_default())
    }

    fn id(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.with_element(this.key, |el| el.attr_atom(dom::atoms::ID).unwrap_or("").to_string()).unwrap_or_default())
    }

    fn set_id(&mut self, this: ObjectRef, value: String) -> Result<(), HostError> {
        self.set_attribute_on(this.key, "id", &value);
        Ok(())
    }

    fn get_attribute(&mut self, this: ObjectRef, qualified_name: String) -> Result<Option<String>, HostError> {
        Ok(self.with_element(this.key, |el| el.attr(&qualified_name).map(str::to_string)).flatten())
    }

    fn set_attribute(&mut self, this: ObjectRef, qualified_name: String, value: String) -> Result<(), HostError> {
        self.set_attribute_on(this.key, &qualified_name, &value);
        Ok(())
    }

    fn remove_attribute(&mut self, this: ObjectRef, qualified_name: String) -> Result<(), HostError> {
        self.remove_attribute_on(this.key, &qualified_name);
        Ok(())
    }

    fn style(&mut self, this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(ObjectRef::new(Interface::CSSStyleDeclaration, this.key))
    }

    // Milestone 19: a checkbox's or radio's state is the presence of its
    // `checked` attribute (HTML's boolean-attribute convention).
    fn checked(&mut self, this: ObjectRef) -> Result<bool, HostError> {
        Ok(self.with_element(this.key, |el| el.attr_atom(dom::atoms::CHECKED).is_some()).unwrap_or(false))
    }

    fn set_checked(&mut self, this: ObjectRef, value: bool) -> Result<(), HostError> {
        if value {
            self.set_attribute_on(this.key, "checked", "checked");
        } else {
            self.remove_attribute_on(this.key, "checked");
        }
        Ok(())
    }
}

impl TextImpl for HostState {}

impl HostState {
    fn style_get(&self, this: ObjectRef, property: usize) -> String {
        let name = STYLE_PROPERTIES[property];
        self.with_element(this.key, |el| el.attr_atom(dom::atoms::STYLE).and_then(|style| find_declaration_value(style, name))).flatten().unwrap_or_default()
    }

    fn style_set(&mut self, this: ObjectRef, property: usize, value: &str) {
        let name = STYLE_PROPERTIES[property];
        if self.attached(this.key) {
            if self.document.borrow_mut().set_inline_style_property(this.key, name, value) {
                self.dirty = true;
            }
        } else {
            self.forest.with_mut(this.key, |el| el.set_inline_style_property(name, value));
        }
    }
}

macro_rules! style_properties {
    ($($get:ident, $set:ident = $index:expr;)*) => {
        impl CSSStyleDeclarationImpl for HostState {
            $(
                fn $get(&mut self, this: ObjectRef) -> Result<String, HostError> {
                    Ok(self.style_get(this, $index))
                }
                fn $set(&mut self, this: ObjectRef, value: String) -> Result<(), HostError> {
                    self.style_set(this, $index, &value);
                    Ok(())
                }
            )*
        }
    };
}

style_properties! {
    color, set_color = 0;
    background_color, set_background_color = 1;
    width, set_width = 2;
    height, set_height = 3;
    font_size, set_font_size = 4;
    text_align, set_text_align = 5;
    transform, set_transform = 6;
}

fn find_declaration_value(style_attr: &str, property: &str) -> Option<String> {
    style_attr.split(';').find_map(|decl| {
        let (k, v) = decl.split_once(':')?;
        (k.trim() == property).then(|| v.trim().to_string())
    })
}

impl WindowImpl for HostState {
    fn window(&mut self, _this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(WINDOW)
    }

    fn self_(&mut self, _this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(WINDOW)
    }

    fn document(&mut self, _this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(DOCUMENT)
    }

    fn local_storage(&mut self, _this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(LOCAL_STORAGE)
    }

    fn session_storage(&mut self, _this: ObjectRef) -> Result<ObjectRef, HostError> {
        Ok(SESSION_STORAGE)
    }

    /// Milestone 14: runs on the next animation frame, not this one.
    fn request_animation_frame(&mut self, _this: ObjectRef, callback: CallbackId) -> Result<u32, HostError> {
        self.animation_frames.push(callback);
        self.next_animation_frame = self.next_animation_frame.wrapping_add(1);
        Ok(self.next_animation_frame)
    }

    fn fetch(&mut self, _this: ObjectRef, input: String, init: RequestInit) -> Result<PromiseValue, HostError> {
        Ok(net::fetch(self, &input, init))
    }
}

impl StorageImpl for HostState {
    fn get_item(&mut self, this: ObjectRef, key: String) -> Result<Option<String>, HostError> {
        Ok(self.storage(this).area()?.get(&key))
    }

    fn set_item(&mut self, this: ObjectRef, key: String, value: String) -> Result<(), HostError> {
        // Like a browser: over the quota, `setItem` throws and the area is
        // unchanged (the `storage` crate reports the guard).
        self.storage(this).area()?.set(key, value).map_err(|storage::QuotaExceeded| {
            HostError::error(format!("QuotaExceededError: the storage quota ({} bytes) would be exceeded", storage::MAX_STORAGE_BYTES))
        })
    }

    fn remove_item(&mut self, this: ObjectRef, key: String) -> Result<(), HostError> {
        self.storage(this).area()?.remove(&key);
        Ok(())
    }

    fn clear(&mut self, this: ObjectRef) -> Result<(), HostError> {
        self.storage(this).area()?.clear();
        Ok(())
    }
}

impl HostState {
    fn storage(&self, this: ObjectRef) -> &StorageSource {
        if this == SESSION_STORAGE { &self.session_storage } else { &self.local_storage }
    }
}

impl ConsoleImpl for HostState {
    fn log(&mut self, data: Vec<Value>) -> Result<(), HostError> {
        let parts: Vec<String> = data.into_iter().map(|v| crate::bindings::convert::string(v).unwrap_or_else(|_| "[object]".into())).collect();
        // Standard error, never standard output: a renderer process's stdout
        // is its channel to the browser (ADR 016), and page content must not
        // be able to write to it.
        eprintln!("{}", parts.join(" "));
        Ok(())
    }
}
