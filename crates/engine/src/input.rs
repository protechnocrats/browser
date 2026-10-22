//! Loading documents, landing navigations, and handling input — what
//! `app/src/main.rs`'s `tick` and `handle_input` closures did before
//! Milestone 29 moved it behind the engine API.

use crate::frame::{collect_focusable_ids, cursor_index_for_click_x, resolve_navigation_target};
use crate::loader::{collect_style_text, navigate_async_with};
use crate::page::Page;
use crate::{EngineEvent, EngineInput, LoadRequest, PageId, Viewport};

/// Starts loading `request` into `page`: a URL becomes a background
/// navigation (the old document stays until it lands); an HTML string is
/// parsed and committed right away.
pub(crate) fn load(page: &Page, id: PageId, request: LoadRequest, events: &mut Vec<EngineEvent>) {
    match request {
        LoadRequest::Url(url) => {
            // A typed/startup load: browser-initiated, no initiator.
            *page.pending_nav.borrow_mut() = Some((url.clone(), navigate_async_with(&url, page.network.clone(), None, page.image_loading.get())));
            events.push(EngineEvent::NavigationStarted { page: id, url });
        }
        LoadRequest::Html { html, url } => {
            let Ok(root) = html_parser::parse_document(&html) else {
                events.push(EngineEvent::NavigationFailed {
                    page: id,
                    url: url.unwrap_or_default(),
                    error: "the document has no <html> element".to_string(),
                });
                return;
            };
            let mut style_text = String::new();
            collect_style_text(dom::ElementRef::from(&root), &mut style_text);
            let sheet = css_parser::parse_stylesheet(&style_text);
            match page.replace_with_navigation(root, sheet, images::ImageCache::new(), Vec::new(), url.clone()) {
                Ok(()) => events.push(EngineEvent::NavigationCommitted { page: id, url }),
                Err(()) => events.push(EngineEvent::NavigationFailed {
                    page: id,
                    url: url.unwrap_or_default(),
                    error: "the document has no <body> element".to_string(),
                }),
            }
        }
    }
}

/// If `page`'s in-flight navigation has finished, commits it (or reports
/// its failure). Returns whether anything landed.
pub(crate) fn land_navigation(page: &Page, id: PageId, events: &mut Vec<EngineEvent>) -> bool {
    let result = page.pending_nav.borrow().as_ref().and_then(|(_, handle)| handle.try_recv());
    let Some(result) = result else { return false };
    let url = page.pending_nav.borrow_mut().take().map(|(url, _)| url).unwrap_or_default();
    match result {
        Ok((root, sheet, cache, pending)) => match page.replace_with_navigation(root, sheet, cache, pending, Some(url.clone())) {
            Ok(()) => events.push(EngineEvent::NavigationCommitted { page: id, url: Some(url) }),
            Err(()) => events.push(EngineEvent::NavigationFailed { page: id, url, error: "the document has no <body> element".to_string() }),
        },
        Err(error) => events.push(EngineEvent::NavigationFailed { page: id, url, error }),
    }
    true
}

/// Handles one input event. Returns whether it may have changed what is on
/// screen (so a frame is needed).
pub(crate) fn handle(page: &Page, id: PageId, viewport: Viewport, input: EngineInput, font: &text::Font, events: &mut Vec<EngineEvent>) -> bool {
    match input {
        // A move is remembered for wheel scrolling, and — when the
        // stylesheet has `:hover` rules (M4's gap) — hit-tested: a new
        // hovered element is worth a frame.
        EngineInput::PointerMove { x, y } => {
            if !(x.is_finite() && y.is_finite()) {
                return false;
            }
            page.pointer.set(Some((x, y)));
            if !page.styles.borrow().uses_interaction() {
                return false;
            }
            let path = hit_path(page, x, y);
            let changed = *page.hover_path.borrow() != path;
            *page.hover_path.borrow_mut() = path;
            changed
        }
        EngineInput::PointerDown { x, y } => {
            if !(x.is_finite() && y.is_finite()) {
                return false;
            }
            if page.styles.borrow().uses_interaction() {
                let path = hit_path(page, x, y);
                *page.hover_path.borrow_mut() = path.clone();
                *page.active_path.borrow_mut() = path;
            }
            click(page, id, x, y, font, events);
            true
        }
        // Ends `:active` (M4's gap).
        EngineInput::PointerUp { .. } => !std::mem::take(&mut *page.active_path.borrow_mut()).is_empty(),
        EngineInput::Scroll { dx, dy } => {
            if !(dx.is_finite() && dy.is_finite()) {
                return false;
            }
            // Milestone 33: the innermost scroll container under the
            // pointer that can move this way takes the scroll; otherwise
            // the page does.
            if scroll_container(page, dx, dy) {
                page.js_env.borrow_mut().dispatch_scroll();
                return true;
            }
            // Clamped to the document: 0 at the top, and at the bottom the
            // last content line sits at the bottom of the viewport.
            let content_height = page.last_layout.borrow().as_ref().map_or(0.0, |root| root.y + root.height);
            let max_offset = (content_height - viewport.height).max(0.0);
            let offset = (page.scroll_offset_y.get() + dy).clamp(0.0, max_offset);
            page.scroll_offset_y.set(offset);
            page.js_env.borrow_mut().dispatch_scroll();
            true
        }
        EngineInput::Key { key } => {
            if key == "Tab" {
                // Tab order is recomputed from the CURRENT document each
                // time: JS can add or remove focusable elements at will.
                let mut tab_order = Vec::new();
                collect_focusable_ids(page.document.borrow().root(), &mut tab_order);
                page.js_env.borrow_mut().focus_next_in_tab_order(&tab_order);
            } else {
                page.js_env.borrow_mut().dispatch_keydown(&key);
            }
            true
        }
    }
}

/// Scrolls the innermost scroll container under the pointer that can still
/// move in the wheel's direction, clamped to its content. Returns whether
/// one did.
fn scroll_container(page: &Page, dx: f32, dy: f32) -> bool {
    let Some((px, py)) = page.pointer.get() else { return false };
    let layout = page.last_layout.borrow();
    let Some(root) = layout.as_ref() else { return false };
    let mut scrolls = page.container_scrolls.borrow_mut();
    let hit = layout::hit_test_counted(root, px, py, page.scroll_offset_y.get(), &scrolls);
    // The boxes the point hit (Milestone 39b: no search of the tree).
    for (&key, placed) in hit.path.iter().zip(&hit.boxes) {
        let b = placed.layout;
        if b.element_key != Some(key) || b.text.is_some() || !b.overflow.scrollable {
            continue;
        }
        let (max_x, max_y) = b.scroll_range();
        let (sx, sy) = scrolls.get(&key).copied().unwrap_or((0.0, 0.0));
        let next = ((sx + dx).clamp(0.0, max_x), (sy + dy).clamp(0.0, max_y));
        if next != (sx, sy) {
            scrolls.insert(key, next);
            return true;
        }
    }
    false
}

/// A click at viewport position (`x`, `y`): hit-test, move the text cursor
/// if it landed in an editable field, dispatch the DOM `click`, and start a
/// navigation if its default action is one.
/// The element under `(x, y)` (viewport coordinates) and its ancestors,
/// target first — empty over nothing, or before the first layout.
fn hit_path(page: &Page, x: f32, y: f32) -> Vec<dom::NodeId> {
    match page.last_layout.borrow().as_ref() {
        Some(layout) => layout::hit_test_counted(layout, x, y, page.scroll_offset_y.get(), &page.container_scrolls.borrow()).path,
        None => Vec::new(),
    }
}

fn click(page: &Page, id: PageId, x: f32, y: f32, font: &text::Font, events: &mut Vec<EngineEvent>) {
    let hit_path = match page.last_layout.borrow().as_ref() {
        Some(layout) => {
            // Scroll-aware: `fixed` boxes do not scroll, `sticky` ones move.
            let hit = layout::hit_test_counted(layout, x, y, page.scroll_offset_y.get(), &page.container_scrolls.borrow());
            // Click-to-position runs BEFORE `dispatch_click`, so a click
            // listener sees the cursor already moved (the same "native
            // behaviour first" order checkbox toggling uses).
            if let Some(&target_key) = hit.path.first() {
                let editable = {
                    let env = page.js_env.borrow();
                    let document = env.document();
                    let document = document.borrow();
                    document
                        .find_by_node_id(target_key)
                        .and_then(|el| el.is_text_editable().then(|| (el.attr_atom(dom::atoms::ID).map(str::to_string), el.editable_value())))
                };
                if let Some((Some(element_id), value)) = editable {
                    // The box the click hit (Milestone 39b: found by the hit
                    // test, not a search), in document space.
                    if let Some(input_box) = hit.boxes.first().map(|b| b.with_children_in_document_space()) {
                        let index = cursor_index_for_click_x(&input_box, &value, x, font);
                        page.js_env.borrow_mut().set_cursor_position(&element_id, index);
                    }
                }
            }
            hit.path
        }
        None => return,
    };
    if !hit_path.is_empty() {
        page.js_env.borrow_mut().dispatch_click(&hit_path);
    }
    // A navigation request — an `<a href>` click or a GET `<form>` submit
    // that was not `preventDefault()`ed. A newer one replaces an older
    // in-flight one, as in browsers.
    let pending = page.js_env.borrow_mut().take_pending_navigation();
    if let Some((action, query)) = pending {
        let target = if query.is_empty() { action } else { format!("{action}?{query}") };
        if let Some(resolved) = resolve_navigation_target(&page.current_url.borrow(), &target) {
            let initiator = page.js_env.borrow().page().clone();
            *page.pending_nav.borrow_mut() =
                Some((resolved.clone(), navigate_async_with(&resolved, page.network.clone(), Some(initiator), page.image_loading.get())));
            events.push(EngineEvent::NavigationStarted { page: id, url: resolved });
        }
    }
}
