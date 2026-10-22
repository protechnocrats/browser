//! Events (Milestones 15, 16, 19, 20, 21): dispatch through capture, target
//! and bubble phases, default actions (checkbox and radio toggles, form
//! submission, link and form navigation), native text editing, focus and tab
//! order, client-side validation. Since Milestone 40 targets are nodes named
//! by `NodeId` (an element needs no `id` to have listeners) and
//! `event.target` is the target's wrapper.

use crate::bindings::{EventImpl, Interface};
use crate::environment::ScriptEnvironment;
use crate::host::{HostObject, HostState, DOCUMENT};
use crate::{HostError, ObjectRef, Value};
use dom::{Document, ElementRef};

/// One dispatched event, as its wrapper reaches it.
pub(crate) struct EventData {
    r#type: String,
    target: Option<ObjectRef>,
    key: Option<String>,
    stopped: bool,
    prevented: bool,
}

/// The listener map's key for `target`: nodes by id, whatever their
/// concrete interface.
pub(crate) fn listener_key(target: ObjectRef) -> ObjectRef {
    if target.interface.inherits(Interface::Node) {
        ObjectRef::new(Interface::Node, target.key)
    } else {
        target
    }
}

impl HostState {
    fn event(&self, this: ObjectRef) -> Result<&EventData, HostError> {
        match self.object(this.key) {
            Some(HostObject::Event(e)) => Ok(e),
            _ => Err(HostError::type_error("the event is gone")),
        }
    }

    fn event_mut(&mut self, this: ObjectRef) -> Result<&mut EventData, HostError> {
        match self.object_mut(this.key) {
            Some(HostObject::Event(e)) => Ok(e),
            _ => Err(HostError::type_error("the event is gone")),
        }
    }
}

impl EventImpl for HostState {
    fn r#type(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.event(this)?.r#type.clone())
    }

    fn target(&mut self, this: ObjectRef) -> Result<Option<ObjectRef>, HostError> {
        Ok(self.event(this)?.target)
    }

    fn key(&mut self, this: ObjectRef) -> Result<Option<String>, HostError> {
        Ok(self.event(this)?.key.clone())
    }

    fn default_prevented(&mut self, this: ObjectRef) -> Result<bool, HostError> {
        Ok(self.event(this)?.prevented)
    }

    fn stop_propagation(&mut self, this: ObjectRef) -> Result<(), HostError> {
        self.event_mut(this)?.stopped = true;
        Ok(())
    }

    fn prevent_default(&mut self, this: ObjectRef) -> Result<(), HostError> {
        self.event_mut(this)?.prevented = true;
        Ok(())
    }
}

impl ScriptEnvironment {
    /// Milestone 15: dispatches a `click` at a hit-test result
    /// (`layout::hit_test_path`, target first, ancestors after), through a
    /// capture phase (root to target, Milestone 20), the target, and a
    /// bubble phase. Focus moves to the first element on the path that has
    /// an `id` (a simplification of the focusable-element rules).
    ///
    /// Then, unless a listener called `preventDefault()`, the click's
    /// default action (Milestones 16, 19, 21): a checkbox toggles, a radio
    /// is checked and its group unchecked, a button or submit input submits
    /// its form, a link requests navigation.
    pub fn dispatch_click(&mut self, hit_path: &[dom::NodeId]) {
        self.begin();
        let (chain, focus) = {
            let host = self.host.borrow();
            let doc = self.document.borrow();
            let focus = hit_path.iter().find_map(|&n| doc.find_by_node_id(n).and_then(|el| el.attr_atom(dom::atoms::ID)).map(str::to_string));
            (hit_path.iter().filter_map(|&n| host.node_ref(n)).collect::<Vec<_>>(), focus)
        };
        if let Some(id) = focus {
            self.focused_id = Some(id);
        }

        let prevented = self.dispatch_event(&chain, "click", None);

        if !prevented {
            if let Some(&target_key) = hit_path.first() {
                let target_info = {
                    let doc = self.document.borrow();
                    doc.find_by_node_id(target_key).and_then(|el| {
                        (el.is(dom::atoms::INPUT)).then(|| (el.attr_atom(dom::atoms::TYPE).unwrap_or("").to_string(), el.attr_atom(dom::atoms::NAME).map(str::to_string)))
                    })
                };
                if let Some((input_type, name)) = target_info {
                    let mut doc = self.document.borrow_mut();
                    match input_type.as_str() {
                        "checkbox" => {
                            let checked = doc.find_by_node_id(target_key).map(|el| el.attr_atom(dom::atoms::CHECKED).is_some());
                            match checked {
                                Some(true) => doc.remove_attribute(target_key, "checked"),
                                Some(false) => doc.set_attribute(target_key, "checked", "checked"),
                                None => false,
                            };
                            self.host.borrow_mut().dirty = true;
                        }
                        "radio" => {
                            if let Some(group_name) = &name {
                                uncheck_radio_group(&mut doc, group_name);
                            }
                            doc.set_attribute(target_key, "checked", "checked");
                            self.host.borrow_mut().dirty = true;
                        }
                        _ => {}
                    }
                }
            }

            let is_submit_control = hit_path.first().map(|&key| {
                let doc = self.document.borrow();
                doc.find_by_node_id(key).is_some_and(|el| el.is(dom::atoms::BUTTON) || (el.is(dom::atoms::INPUT) && el.attr_atom(dom::atoms::TYPE) == Some("submit")))
            });
            if is_submit_control == Some(true) {
                self.dispatch_submit_internal(hit_path);
            }

            // Milestone 21: `<a href>` navigation is a click's default action
            // too; a script router suppresses it with `preventDefault()`.
            // Only what was requested is recorded; the engine resolves it.
            if let Some(&target_key) = hit_path.first() {
                let doc = self.document.borrow();
                if let Some(href) = doc.find_by_node_id(target_key).filter(|el| el.is(dom::atoms::A)).and_then(|el| el.attr_atom(dom::atoms::HREF)) {
                    self.pending_navigation = Some((href.to_string(), String::new()));
                }
            }
        }

        self.run_jobs();
    }

    /// Milestone 15/16: dispatches `keydown` to the focused element (and the
    /// document's listeners, focused or not), then — unless prevented —
    /// applies native text editing (Milestone 20 put it after dispatch).
    pub fn dispatch_keydown(&mut self, key: &str) {
        self.begin();
        let chain: Vec<ObjectRef> = {
            let host = self.host.borrow();
            let doc = self.document.borrow();
            self.focused_id.as_deref().and_then(|id| doc.node_id_of(id)).and_then(|n| host.node_ref(n)).into_iter().collect()
        };
        let prevented = self.dispatch_event_with_key(&chain, "keydown", Some(key));
        if !prevented {
            self.apply_native_text_editing(key);
            if key == "Enter" {
                self.enter_default_action();
            }
        }
        self.run_jobs();
    }

    /// Enter's default action. On a focused button it is a click (its
    /// activation behaviour). In a form's text field it submits the form
    /// (HTML §4.10.21.2, "implicit submission"): by a click on the form's
    /// default button — its first submit button, which does nothing if
    /// disabled — or, with no such button, directly when the field is the
    /// only one in the form that blocks implicit submission.
    fn enter_default_action(&mut self) {
        enum Action {
            Click(Vec<dom::NodeId>),
            Submit(Vec<dom::NodeId>),
        }
        let action = {
            let doc = self.document.borrow();
            let Some(field) = self.focused_id.as_deref().and_then(|id| doc.find_by_id(id)) else { return };
            let path_of = |el: ElementRef<'_>| std::iter::once(el.node_id()).chain(doc.ancestors_of(el.node_id()).iter().map(|a| a.node_id())).collect::<Vec<_>>();
            let action = if is_button(field) {
                if field.attr_atom(dom::atoms::DISABLED).is_some() {
                    return;
                }
                Action::Click(path_of(field))
            } else {
                if !blocks_implicit_submission(field) {
                    return;
                }
                let Some(form) = doc.ancestors_of(field.node_id()).into_iter().find(|a| a.is(dom::atoms::FORM)) else { return };
                match form.descendants_and_self().find(|el| is_submit_button(*el)) {
                    Some(button) if button.attr_atom(dom::atoms::DISABLED).is_some() => return,
                    Some(button) => Action::Click(path_of(button)),
                    None if form.descendants_and_self().filter(|el| blocks_implicit_submission(*el)).count() == 1 => Action::Submit(path_of(field)),
                    None => return,
                }
            };
            action
        };
        match action {
            Action::Click(path) => self.dispatch_click(&path),
            Action::Submit(path) => self.dispatch_submit_internal(&path),
        }
    }

    /// Milestone 16: `<input>`/`<textarea>`/`contenteditable` editing. An
    /// input's current text is its `value` attribute (no attribute/property
    /// split); a `contenteditable` element's is its text, collapsed to one
    /// text node when edited (basic editing; rich structure is out of
    /// scope). `Tab` is the engine's, through [`Self::focus_next_in_tab_order`].
    fn apply_native_text_editing(&mut self, key: &str) {
        let Some(id) = self.focused_id.clone() else { return };
        // `dom`'s rule is the one definition of editable (ADR 021 §6).
        let (editable, is_contenteditable) = {
            let doc = self.document.borrow();
            match doc.find_by_id(&id) {
                Some(el) => (el.is_text_editable(), el.tag() != "input" && el.tag() != "textarea"),
                None => (false, false),
            }
        };
        if !editable {
            return;
        }

        let cursor = self.cursor_positions.entry(id.clone()).or_insert(0);
        let mut doc = self.document.borrow_mut();
        let Some(node) = doc.node_id_of(&id) else { return };
        let Some(el) = doc.find_by_node_id(node) else { return };
        let original = el.editable_value();
        let mut value: Vec<char> = original.chars().collect();
        *cursor = (*cursor).min(value.len());

        match key {
            "Backspace" => {
                if *cursor > 0 {
                    value.remove(*cursor - 1);
                    *cursor -= 1;
                }
            }
            "Delete" => {
                if *cursor < value.len() {
                    value.remove(*cursor);
                }
            }
            "ArrowLeft" => *cursor = cursor.saturating_sub(1),
            "ArrowRight" => *cursor = (*cursor + 1).min(value.len()),
            "Home" => *cursor = 0,
            "End" => *cursor = value.len(),
            _ if key.chars().count() == 1 && !key.chars().next().is_some_and(char::is_control) => {
                if let Some(ch) = key.chars().next() {
                    value.insert(*cursor, ch);
                    *cursor += 1;
                }
            }
            _ => {} // Enter, Tab, Shift, other named keys: no effect on the text
        }

        let new_value: String = value.into_iter().collect();
        // Only a real text change is a DOM mutation: a caret move must not
        // rewrite the value (that cost a restyle and relayout, M31).
        if new_value != original {
            if is_contenteditable {
                doc.set_text_content(node, &new_value);
            } else {
                doc.set_attribute(node, "value", new_value);
            }
        }
        // A caret move needs a redraw too.
        self.host.borrow_mut().dirty = true;
    }

    /// The caret's position (a character index) in a text field; `0` for
    /// one never edited.
    pub fn cursor_position(&self, element_id: &str) -> usize {
        self.cursor_positions.get(element_id).copied().unwrap_or(0)
    }

    /// Milestone 19: sets the caret (click-to-position; the engine does the
    /// pixel math), clamped to the field's current length.
    pub fn set_cursor_position(&mut self, element_id: &str, pos: usize) {
        let value_len = self
            .document
            .borrow()
            .find_by_id(element_id)
            .map(|el| {
                // A `contenteditable` element's value is its text; anything
                // else's is its `value` attribute (ADR 021 §6).
                let is_contenteditable = el.is_text_editable() && !matches!(el.tag(), "input" | "textarea");
                if is_contenteditable { el.text_content().chars().count() } else { el.attr_atom(dom::atoms::VALUE).unwrap_or("").chars().count() }
            })
            .unwrap_or(0);
        self.cursor_positions.insert(element_id.to_string(), pos.min(value_len));
    }

    /// Milestone 16: focus moves to the next id in `tab_order` (document
    /// order, built by the engine), wrapping around; to the first if nothing
    /// in the list is focused.
    pub fn focus_next_in_tab_order(&mut self, tab_order: &[String]) {
        if tab_order.is_empty() {
            return;
        }
        let next_index = match self.focused_id.as_ref().and_then(|id| tab_order.iter().position(|t| t == id)) {
            Some(i) => (i + 1) % tab_order.len(),
            None => 0,
        };
        self.focused_id = Some(tab_order[next_index].clone());
    }

    /// Milestone 16: fires `submit` at the form on `form_ancestor_path`
    /// (target first, ancestors after), if its fields are valid (Milestone
    /// 20), then — unless prevented — requests a GET form's navigation
    /// (Milestone 21; POST bodies are not built).
    pub fn dispatch_submit(&mut self, form_ancestor_path: &[dom::NodeId]) {
        self.begin();
        self.dispatch_submit_internal(form_ancestor_path);
        self.run_jobs();
    }

    fn dispatch_submit_internal(&mut self, form_ancestor_path: &[dom::NodeId]) {
        let (form_valid, form_at) = {
            let doc = self.document.borrow();
            let form_at = form_ancestor_path.iter().position(|&key| doc.find_by_node_id(key).is_some_and(|el| el.is(dom::atoms::FORM)));
            let valid = form_at.and_then(|i| doc.find_by_node_id(form_ancestor_path[i])).map(form_is_valid).unwrap_or(true);
            (valid, form_at)
        };
        if !form_valid {
            return;
        }
        let Some(form_at) = form_at else { return }; // no enclosing form: nothing to submit
        let chain: Vec<ObjectRef> = {
            let host = self.host.borrow();
            form_ancestor_path[form_at..].iter().filter_map(|&n| host.node_ref(n)).collect()
        };
        let prevented = self.dispatch_event(&chain, "submit", None);

        if !prevented {
            let form_info = {
                let doc = self.document.borrow();
                doc.find_by_node_id(form_ancestor_path[form_at]).and_then(|el| {
                    let method = el.attr_atom(dom::atoms::METHOD).map(str::to_ascii_lowercase).unwrap_or_else(|| "get".to_string());
                    (method == "get").then(|| (el.attr_atom(dom::atoms::ACTION).unwrap_or("").to_string(), build_form_query_string(el)))
                })
            };
            if let Some((action, query)) = form_info {
                self.pending_navigation = Some((action, query));
            }
        }
    }

    /// Milestone 15: `scroll` goes to the document's listeners (the document
    /// scrolls as a whole), with no event object.
    pub fn dispatch_scroll(&mut self) {
        self.begin();
        for callback in self.listeners_of(DOCUMENT, "scroll", None) {
            if let Err(e) = self.runtime.call(callback, &Value::Object(DOCUMENT), &[]) {
                crate::report_script_error("event_callback", &e);
            }
        }
        self.sync();
        self.run_jobs();
    }

    fn dispatch_event(&mut self, chain: &[ObjectRef], event_type: &str, key: Option<&str>) -> bool {
        self.dispatch_event_with_key(chain, event_type, key)
    }

    /// Milestone 20: fires one event object through the capture phase
    /// (document, then ancestors down to the target's parent; capture
    /// listeners only), the target (all of its listeners) and the bubble
    /// phase (ancestors, then the document; bubble listeners only),
    /// stopping where a listener calls `stopPropagation()`. `chain` is
    /// target first. Returns whether a listener called `preventDefault()`.
    fn dispatch_event_with_key(&mut self, chain: &[ObjectRef], event_type: &str, key: Option<&str>) -> bool {
        let event_key = {
            let mut host = self.host.borrow_mut();
            let data = EventData { r#type: event_type.to_string(), target: chain.first().copied(), key: key.map(str::to_string), stopped: false, prevented: false };
            let k = host.new_object(HostObject::Event(data));
            host.pin(k);
            k
        };
        let event = Value::Object(ObjectRef::new(Interface::Event, event_key));
        let full_chain: Vec<ObjectRef> = chain.iter().copied().chain(std::iter::once(DOCUMENT)).collect();
        let stopped = |env: &Self| matches!(env.host.borrow().object(event_key), Some(HostObject::Event(e)) if e.stopped);

        for &target in full_chain[1..].iter().rev() {
            if stopped(self) {
                break;
            }
            self.invoke(target, event_type, &event, Some(true));
        }
        for (i, &target) in full_chain.iter().enumerate() {
            if stopped(self) {
                break;
            }
            self.invoke(target, event_type, &event, if i == 0 { None } else { Some(false) });
        }
        let prevented = matches!(self.host.borrow().object(event_key), Some(HostObject::Event(e)) if e.prevented);
        self.host.borrow_mut().unpin(event_key);
        self.sync();
        prevented
    }

    /// The listeners of `target` for `event_type`: all of them
    /// (`capture: None`), or the capture or bubble ones only.
    fn listeners_of(&self, target: ObjectRef, event_type: &str, capture: Option<bool>) -> Vec<crate::CallbackId> {
        let host = self.host.borrow();
        host.listeners
            .get(&listener_key(target))
            .and_then(|by_type| by_type.get(event_type))
            .map(|v| v.iter().filter(|(_, c)| capture.is_none_or(|want| *c == want)).map(|(f, _)| *f).collect())
            .unwrap_or_default()
    }

    fn invoke(&mut self, target: ObjectRef, event_type: &str, event: &Value, capture: Option<bool>) {
        for callback in self.listeners_of(target, event_type, capture) {
            if let Err(e) = self.runtime.call(callback, &Value::Object(target), std::slice::from_ref(event)) {
                crate::report_script_error("event_listener", &e);
            }
        }
    }
}

/// Milestone 19: unchecks every radio of the group `name`. One recorded walk
/// (`Document::mutate_elements`), once per radio click.
fn uncheck_radio_group(document: &mut Document, name: &str) {
    document.mutate_elements(|el| {
        let in_group = el.is(dom::atoms::INPUT) && el.attr_atom(dom::atoms::TYPE) == Some("radio") && el.attr_atom(dom::atoms::NAME) == Some(name);
        if in_group && el.attr_atom(dom::atoms::CHECKED).is_some() {
            el.remove_attr("checked");
            true
        } else {
            false
        }
    });
}

/// Milestone 21: the `application/x-www-form-urlencoded` query of a GET
/// form: every named `input`/`textarea`/`select` in document order, unchecked
/// checkboxes and radios left out.
fn build_form_query_string(form: ElementRef<'_>) -> String {
    let mut pairs = Vec::new();
    collect_form_fields(form, &mut pairs);
    pairs.iter().map(|(k, v)| format!("{}={}", url_encode_form_value(k), url_encode_form_value(v))).collect::<Vec<_>>().join("&")
}

fn collect_form_fields(element: ElementRef<'_>, out: &mut Vec<(String, String)>) {
    if matches!(element.tag(), "input" | "textarea" | "select") {
        if let Some(name) = element.attr_atom(dom::atoms::NAME) {
            let is_checkable = element.is(dom::atoms::INPUT) && matches!(element.attr_atom(dom::atoms::TYPE), Some("checkbox") | Some("radio"));
            if !is_checkable || element.attr_atom(dom::atoms::CHECKED).is_some() {
                out.push((name.to_string(), element.attr_atom(dom::atoms::VALUE).unwrap_or("").to_string()));
            }
        }
    }
    for el in element.element_children() {
        collect_form_fields(el, out);
    }
}

/// `application/x-www-form-urlencoded` byte encoding: unreserved characters
/// as they are, space as `+`, everything else percent-encoded.
fn url_encode_form_value(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Milestone 20: whether every `required`/`pattern` field in `form` passes
/// (`dom`'s rule, shared with `style` and `accessibility`, ADR 021 §6).
/// A submit button (HTML §4.10.6, §4.10.5.1.18): a `<button>` whose type is
/// missing, invalid or `submit`, or an `<input type=submit|image>`.
fn is_submit_button(el: ElementRef<'_>) -> bool {
    let kind = el.attr_atom(dom::atoms::TYPE).map(str::to_ascii_lowercase);
    if el.is(dom::atoms::BUTTON) {
        return !matches!(kind.as_deref(), Some("button" | "reset"));
    }
    el.is(dom::atoms::INPUT) && matches!(kind.as_deref(), Some("submit" | "image"))
}

/// A button of any kind: `<button>`, or `<input type=submit|image|button|reset>`.
fn is_button(el: ElementRef<'_>) -> bool {
    el.is(dom::atoms::BUTTON) || (el.is(dom::atoms::INPUT) && matches!(el.attr_atom(dom::atoms::TYPE).map(str::to_ascii_lowercase).as_deref(), Some("submit" | "image" | "button" | "reset")))
}

/// A field that blocks implicit submission (HTML §4.10.21.2): an `<input>`
/// of a text-like type, the default type included.
fn blocks_implicit_submission(el: ElementRef<'_>) -> bool {
    el.is(dom::atoms::INPUT)
        && matches!(
            el.attr_atom(dom::atoms::TYPE).map(str::to_ascii_lowercase).as_deref(),
            None | Some("text" | "search" | "url" | "tel" | "email" | "password" | "date" | "month" | "week" | "time" | "datetime-local" | "number")
        )
}

fn form_is_valid(form: ElementRef<'_>) -> bool {
    form.is_valid() && form.element_children().all(form_is_valid)
}
