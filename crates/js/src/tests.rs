    use super::*;
    use dom::{Element, Node};
    use network::{FetchResponse, Origin};
    use network::NetworkContext;
    use storage::StorageStore as LocalStorageStore;

    fn make_doc() -> Rc<RefCell<Document>> {
        let mut root = Element::new("div");
        let mut headline = Element::new("h1");
        headline.attributes.push(("id".into(), "headline".into()));
        headline.children.push(Node::Text("original".to_string()));
        root.children.push(Node::Element(headline));
        Rc::new(RefCell::new(Document::new(root)))
    }

    // ---- Milestone 11: script execution + DOM bindings ----

    #[test]
    fn runs_a_simple_script_and_returns_its_value() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("1 + 1").unwrap();
        assert_eq!(result, "2");
    }

    #[test]
    fn script_error_does_not_panic_and_is_reported() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("this is not valid javascript {{{");
        assert!(result.is_err());
    }

    #[test]
    fn thrown_exception_does_not_panic_and_is_reported() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("throw new Error('boom')");
        assert!(result.is_err());
    }

    #[test]
    fn get_element_by_id_reads_text_content() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("document.getElementById('headline').textContent").unwrap();
        assert_eq!(result, "original");
    }

    #[test]
    fn get_element_by_id_on_missing_id_returns_null_not_a_panic() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("document.getElementById('nope') === null").unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn set_text_content_mutates_the_shared_dom() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('headline').textContent = 'changed'").unwrap();

        // Verify through the SAME Rc the caller holds — this is the
        // entire point of Milestone 13's architecture: JS and the
        // render loop share one tree, not a copy.
        assert_eq!(doc.borrow().find_by_id("headline").unwrap().text_content(), "changed");
    }

    #[test]
    fn query_selector_finds_by_tag() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("document.querySelector('h1').textContent").unwrap();
        assert_eq!(result, "original");
    }

    #[test]
    fn query_selector_no_match_returns_null() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("document.querySelector('span') === null").unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn style_property_get_and_set_roundtrip() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('headline').style.color = 'red'").unwrap();

        let style = doc.borrow().find_by_id("headline").unwrap().attr("style").unwrap().to_string();
        assert!(style.contains("color:red"));

        let read_back = env.run_script("document.getElementById('headline').style.color").unwrap();
        assert_eq!(read_back, "red");
    }

    #[test]
    fn console_log_does_not_panic() {
        let mut env = JsEnvironment::new(make_doc());
        // The only real assertion is that this doesn't panic — stdout
        // output itself isn't captured by the test harness.
        env.run_script("console.log('hello', 1, true)").unwrap();
    }

    #[test]
    fn append_child_adds_a_new_element_and_sets_dirty() {
        // Milestone 18: the real two-step DOM signature — create,
        // configure, then attach — replacing the old one-call
        // `appendChild('span', 'appended')` form.
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "var el = document.createElement('span'); \
             el.textContent = 'appended'; \
             document.getElementById('headline').appendChild(el);",
        )
        .unwrap();

        let headline = doc.borrow();
        let headline = headline.find_by_id("headline").unwrap();
        assert!(headline.children().any(|c| matches!(c, dom::ChildRef::Element(el) if el.tag() == "span")));
        assert_eq!(headline.text_content(), "originalappended");
    }

    #[test]
    fn append_child_sets_dirty_flag() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script(
            "var el = document.createElement('span'); \
             document.getElementById('headline').appendChild(el);",
        )
        .unwrap();
        assert!(env.take_dirty());
    }

    // ---- Milestone 18: detached DOM nodes & document.createElement ----

    #[test]
    fn created_element_is_detached_until_appended() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "var el = document.createElement('span'); \
             el.setAttribute('id', 'new-span'); \
             el.textContent = 'hi';",
        )
        .unwrap();
        // Configured, but never appended — must not be findable in the
        // live tree yet.
        assert!(doc.borrow().find_by_id("new-span").is_none());
    }

    #[test]
    fn created_element_becomes_findable_after_append_child() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "var el = document.createElement('span'); \
             el.setAttribute('id', 'new-span'); \
             el.textContent = 'hi'; \
             document.getElementById('headline').appendChild(el);",
        )
        .unwrap();
        assert_eq!(doc.borrow().find_by_id("new-span").unwrap().text_content(), "hi");
    }

    #[test]
    fn set_get_and_remove_attribute_round_trip_on_a_detached_element() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env
            .run_script(
                "var el = document.createElement('div'); \
                 var beforeSet = el.getAttribute('class') === null; \
                 el.setAttribute('class', 'x'); \
                 var afterSet = el.getAttribute('class') === 'x'; \
                 el.removeAttribute('class'); \
                 var afterRemove = el.getAttribute('class') === null; \
                 [beforeSet, afterSet, afterRemove].join(',');",
            )
            .unwrap();
        assert_eq!(result, "true,true,true");
    }

    #[test]
    fn appending_a_detached_subtree_attaches_the_whole_thing() {
        // Build parent+child while BOTH are still detached, then attach
        // only the parent — the child (nested inside it) must come
        // along, findable by its own id afterward.
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "var parent = document.createElement('div'); \
             parent.setAttribute('id', 'new-parent'); \
             var child = document.createElement('span'); \
             child.setAttribute('id', 'new-child'); \
             parent.appendChild(child); \
             document.getElementById('headline').appendChild(parent);",
        )
        .unwrap();
        let root = doc.borrow();
        assert!(root.find_by_id("new-parent").is_some());
        assert!(root.find_by_id("new-child").is_some());
    }

    #[test]
    fn creating_and_appending_many_elements_in_a_loop_works() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "var container = document.getElementById('headline'); \
             for (var i = 0; i < 5; i++) { \
               var el = document.createElement('div'); \
               el.setAttribute('id', 'item-' + i); \
               el.textContent = 'item ' + i; \
               container.appendChild(el); \
             }",
        )
        .unwrap();
        let root = doc.borrow();
        for i in 0..5 {
            assert_eq!(root.find_by_id(&format!("item-{i}")).unwrap().text_content(), format!("item {i}"));
        }
    }

    #[test]
    fn dropping_created_elements_without_appending_does_not_panic() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script(
            "for (var i = 0; i < 5; i++) { document.createElement('div'); } 'ok';",
        );
        assert_eq!(result.unwrap(), "ok");
    }

    #[test]
    fn a_created_elements_handle_keeps_working_after_attachment() {
        // Milestone 40 (ADR 023 §4): one wrapper per node, whether it is in
        // the document or not. (M18's detached wrapper stopped working once
        // its element was attached.)
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let same = env
            .run_script(
                "var el = document.createElement('span'); \
                 el.setAttribute('id', 'new-span'); \
                 document.getElementById('headline').appendChild(el); \
                 el.textContent = 'written after attaching'; \
                 el === document.getElementById('new-span')",
            )
            .unwrap();
        assert_eq!(same, "true", "the created element's handle is its wrapper in the document too");
        assert_eq!(doc.borrow().find_by_id("new-span").unwrap().text_content(), "written after attaching");
    }

    // ---- Milestone 19: checkbox/radio + click-to-position ----

    fn make_doc_with_checkbox() -> Rc<RefCell<Document>> {
        let mut root = Element::new("div");
        let mut checkbox = Element::new("input");
        checkbox.attributes.push(("id".into(), "agree".into()));
        checkbox.attributes.push(("type".into(), "checkbox".into()));
        root.children.push(Node::Element(checkbox));
        Rc::new(RefCell::new(Document::new(root)))
    }

    fn hit_path_for(doc: &Rc<RefCell<Document>>, id: &str) -> Vec<dom::NodeId> {
        vec![(doc.borrow().find_by_id(id).unwrap()).node_id()]
    }

    /// Like `hit_path_for`, but the FULL target-then-ancestors-to-root
    /// chain (matching real `layout::hit_test_path`'s shape) — needed
    /// wherever dispatch has to walk up to find an enclosing element
    /// (e.g. `dispatch_submit_internal` finding the `<form>`).
    fn full_hit_path_for(doc: &Rc<RefCell<Document>>, id: &str) -> Vec<dom::NodeId> {
        fn walk(el: dom::ElementRef<'_>, id: &str, path: &mut Vec<dom::NodeId>) -> bool {
            if el.attr("id") == Some(id) {
                path.push(el.node_id());
                return true;
            }
            for child_el in el.element_children() {
                if walk(child_el, id, path) {
                    path.push(el.node_id());
                    return true;
                }
            }
            false
        }
        let root_ref = doc.borrow();
        let mut path = Vec::new();
        walk(root_ref.root(), id, &mut path);
        path
    }

    #[test]
    fn checked_accessor_reads_and_writes_the_checked_attribute() {
        let doc = make_doc_with_checkbox();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let before = env.run_script("document.getElementById('agree').checked").unwrap();
        assert_eq!(before, "false");

        env.run_script("document.getElementById('agree').checked = true").unwrap();
        assert!(doc.borrow().find_by_id("agree").unwrap().attr("checked").is_some());

        let after = env.run_script("document.getElementById('agree').checked").unwrap();
        assert_eq!(after, "true");

        env.run_script("document.getElementById('agree').checked = false").unwrap();
        assert!(doc.borrow().find_by_id("agree").unwrap().attr("checked").is_none());
    }

    #[test]
    fn clicking_a_checkbox_toggles_it() {
        let doc = make_doc_with_checkbox();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let path = hit_path_for(&doc, "agree");

        env.dispatch_click(&path);
        assert!(doc.borrow().find_by_id("agree").unwrap().attr("checked").is_some());

        env.dispatch_click(&path);
        assert!(doc.borrow().find_by_id("agree").unwrap().attr("checked").is_none());
    }

    #[test]
    fn clicking_a_radio_unchecks_others_in_the_same_group() {
        let mut root = Element::new("div");
        for (id, checked) in [("a", true), ("b", false), ("c", false)] {
            let mut radio = Element::new("input");
            radio.attributes.push(("id".into(), id.into()));
            radio.attributes.push(("type".into(), "radio".into()));
            radio.attributes.push(("name".into(), "choice".into()));
            if checked {
                radio.attributes.push(("checked".into(), "checked".into()));
            }
            root.children.push(Node::Element(radio));
        }
        let doc = Rc::new(RefCell::new(Document::new(root)));
        let mut env = JsEnvironment::new(Rc::clone(&doc));

        env.dispatch_click(&hit_path_for(&doc, "b"));

        let root_ref = doc.borrow();
        assert!(root_ref.find_by_id("a").unwrap().attr("checked").is_none());
        assert!(root_ref.find_by_id("b").unwrap().attr("checked").is_some());
        assert!(root_ref.find_by_id("c").unwrap().attr("checked").is_none());
    }

    #[test]
    fn clicking_a_radio_in_a_different_group_does_not_affect_it() {
        let mut root = Element::new("div");
        let mut a = Element::new("input");
        a.attributes.push(("id".into(), "a".into()));
        a.attributes.push(("type".into(), "radio".into()));
        a.attributes.push(("name".into(), "group1".into()));
        a.attributes.push(("checked".into(), "checked".into()));
        root.children.push(Node::Element(a));
        let mut b = Element::new("input");
        b.attributes.push(("id".into(), "b".into()));
        b.attributes.push(("type".into(), "radio".into()));
        b.attributes.push(("name".into(), "group2".into()));
        root.children.push(Node::Element(b));

        let doc = Rc::new(RefCell::new(Document::new(root)));
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.dispatch_click(&hit_path_for(&doc, "b"));

        let root_ref = doc.borrow();
        assert!(root_ref.find_by_id("a").unwrap().attr("checked").is_some(), "a different radio group must be unaffected");
        assert!(root_ref.find_by_id("b").unwrap().attr("checked").is_some());
    }

    #[test]
    fn set_cursor_position_moves_the_caret_and_clamps_to_value_length() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('headline')").ok(); // no-op, just to have run something
        {
            let mut root = doc.borrow_mut();
            let node = root.node_id_of("headline").unwrap();
            root.set_attribute(node, "value", "hello");
        }
        env.set_cursor_position("headline", 3);
        assert_eq!(env.cursor_position("headline"), 3);

        env.set_cursor_position("headline", 999);
        assert_eq!(env.cursor_position("headline"), 5, "should clamp to the value's character length");
    }

    // ---- Milestone 20: full event model & client-side validation ----

    #[test]
    fn capture_phase_fires_before_target_which_fires_before_bubble() {
        let mut root = dom::Element::new("div");
        root.attributes.push(("id".into(), "outer".into()));
        let mut inner = dom::Element::new("span");
        inner.attributes.push(("id".into(), "inner".into()));
        root.children.push(dom::Node::Element(inner));
        let doc = Rc::new(RefCell::new(Document::new(root)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.order = []; \
             document.getElementById('outer').addEventListener('click', () => globalThis.order.push('outer-capture'), true); \
             document.getElementById('inner').addEventListener('click', () => globalThis.order.push('inner-target')); \
             document.getElementById('outer').addEventListener('click', () => globalThis.order.push('outer-bubble'));",
        )
        .unwrap();

        let inner_key = element_key_for(&doc, "inner");
        let outer_key = element_key_for(&doc, "outer");
        env.dispatch_click(&[inner_key, outer_key]);

        assert_eq!(env.run_script("globalThis.order.join(',')").unwrap(), "outer-capture,inner-target,outer-bubble");
    }

    #[test]
    fn stop_propagation_on_the_target_prevents_the_ancestor_bubble_listener() {
        let mut root = dom::Element::new("div");
        root.attributes.push(("id".into(), "outer".into()));
        let mut inner = dom::Element::new("span");
        inner.attributes.push(("id".into(), "inner".into()));
        root.children.push(dom::Node::Element(inner));
        let doc = Rc::new(RefCell::new(Document::new(root)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.outerFired = false; \
             document.getElementById('inner').addEventListener('click', (e) => e.stopPropagation()); \
             document.getElementById('outer').addEventListener('click', () => { globalThis.outerFired = true; });",
        )
        .unwrap();

        let inner_key = element_key_for(&doc, "inner");
        let outer_key = element_key_for(&doc, "outer");
        env.dispatch_click(&[inner_key, outer_key]);

        assert_eq!(env.run_script("globalThis.outerFired").unwrap(), "false");
    }

    #[test]
    fn event_object_exposes_type_and_target() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.seen = null; \
             document.getElementById('headline').addEventListener('click', (e) => { globalThis.seen = e.type + ':' + e.target.id + ':' + (e.target === document.getElementById('headline')); });",
        )
        .unwrap();
        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]);
        assert_eq!(env.run_script("globalThis.seen").unwrap(), "click:headline:true", "the target is the element's own wrapper");
    }

    #[test]
    fn prevent_default_on_a_checkbox_click_suppresses_the_native_toggle() {
        let doc = make_doc_with_checkbox();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('agree').addEventListener('click', (e) => e.preventDefault());").unwrap();

        env.dispatch_click(&hit_path_for(&doc, "agree"));

        assert!(doc.borrow().find_by_id("agree").unwrap().attr("checked").is_none(), "preventDefault should suppress the native toggle");
    }

    #[test]
    fn prevent_default_on_keydown_suppresses_native_text_editing() {
        let doc = make_doc();
        {
            let mut root = doc.borrow_mut();
            { let node = root.node_id_of("headline").unwrap(); root.set_attribute(node, "value", ""); }
        }
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("headline".to_string()));
        env.run_script("document.getElementById('headline').addEventListener('keydown', (e) => e.preventDefault());").unwrap();

        env.dispatch_keydown("x");

        assert_eq!(doc.borrow().find_by_id("headline").unwrap().attr("value"), Some(""), "preventDefault should suppress native character insertion");
    }

    fn make_doc_with_form_field(required: bool, pattern: Option<&str>) -> Rc<RefCell<Document>> {
        let mut root = Element::new("div");
        let mut form = Element::new("form");
        form.attributes.push(("id".into(), "f".into()));
        let mut input = Element::new("input");
        input.attributes.push(("id".into(), "field".into()));
        if required {
            input.attributes.push(("required".into(), "required".into()));
        }
        if let Some(p) = pattern {
            input.attributes.push(("pattern".into(), p.into()));
        }
        let mut button = Element::new("button");
        button.attributes.push(("id".into(), "go".into()));
        form.children.push(Node::Element(input));
        form.children.push(Node::Element(button));
        root.children.push(Node::Element(form));
        Rc::new(RefCell::new(Document::new(root)))
    }

    #[test]
    fn submit_is_blocked_when_a_required_field_is_empty() {
        let doc = make_doc_with_form_field(true, None);
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.submitted = false; document.getElementById('f').addEventListener('submit', () => { globalThis.submitted = true; });").unwrap();

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "false");
    }

    #[test]
    fn submit_fires_once_the_required_field_is_filled() {
        let doc = make_doc_with_form_field(true, None);
        {
            let mut root = doc.borrow_mut();
            { let node = root.node_id_of("field").unwrap(); root.set_attribute(node, "value", "hi"); }
        }
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.submitted = false; document.getElementById('f').addEventListener('submit', () => { globalThis.submitted = true; });").unwrap();

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "true");
    }

    #[test]
    fn submit_is_blocked_when_a_field_does_not_match_its_pattern() {
        let doc = make_doc_with_form_field(false, Some("[0-9]+"));
        {
            let mut root = doc.borrow_mut();
            { let node = root.node_id_of("field").unwrap(); root.set_attribute(node, "value", "not-digits"); }
        }
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.submitted = false; document.getElementById('f').addEventListener('submit', () => { globalThis.submitted = true; });").unwrap();

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "false");
    }

    #[test]
    fn submit_fires_when_a_field_matches_its_pattern() {
        let doc = make_doc_with_form_field(false, Some("[0-9]+"));
        {
            let mut root = doc.borrow_mut();
            { let node = root.node_id_of("field").unwrap(); root.set_attribute(node, "value", "12345"); }
        }
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.submitted = false; document.getElementById('f').addEventListener('submit', () => { globalThis.submitted = true; });").unwrap();

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "true");
    }

    // ---- Milestone 21: page navigation triggers ----

    fn make_doc_with_link(href: &str) -> Rc<RefCell<Document>> {
        let mut root = Element::new("div");
        let mut a = Element::new("a");
        a.attributes.push(("id".into(), "link".into()));
        a.attributes.push(("href".into(), href.into()));
        root.children.push(Node::Element(a));
        Rc::new(RefCell::new(Document::new(root)))
    }

    #[test]
    fn clicking_a_link_sets_pending_navigation_to_its_href() {
        let doc = make_doc_with_link("https://example.com/other");
        let mut env = JsEnvironment::new(Rc::clone(&doc));

        env.dispatch_click(&hit_path_for(&doc, "link"));

        assert_eq!(env.take_pending_navigation(), Some(("https://example.com/other".to_string(), String::new())));
        // Consumed — a second call finds nothing left.
        assert_eq!(env.take_pending_navigation(), None);
    }

    #[test]
    fn prevent_default_on_a_link_click_suppresses_navigation() {
        let doc = make_doc_with_link("https://example.com/other");
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('link').addEventListener('click', (e) => e.preventDefault());").unwrap();

        env.dispatch_click(&hit_path_for(&doc, "link"));

        assert_eq!(env.take_pending_navigation(), None);
    }

    #[test]
    fn a_non_link_click_does_not_set_pending_navigation() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.dispatch_click(&hit_path_for(&doc, "headline"));
        assert_eq!(env.take_pending_navigation(), None);
    }

    fn make_doc_with_get_form(action: &str) -> Rc<RefCell<Document>> {
        let mut root = Element::new("div");
        let mut form = Element::new("form");
        form.attributes.push(("id".into(), "f".into()));
        if !action.is_empty() {
            form.attributes.push(("action".into(), action.into()));
        }
        let mut q = Element::new("input");
        q.attributes.push(("name".into(), "q".into()));
        q.attributes.push(("value".into(), "hello world".into()));
        let mut agree = Element::new("input");
        agree.attributes.push(("type".into(), "checkbox".into()));
        agree.attributes.push(("name".into(), "agree".into()));
        // deliberately NOT checked — must be excluded from the query string
        let mut button = Element::new("button");
        button.attributes.push(("id".into(), "go".into()));
        form.children.push(Node::Element(q));
        form.children.push(Node::Element(agree));
        form.children.push(Node::Element(button));
        root.children.push(Node::Element(form));
        Rc::new(RefCell::new(Document::new(root)))
    }

    #[test]
    fn submitting_a_get_form_sets_pending_navigation_with_a_query_string() {
        let doc = make_doc_with_get_form("/search");
        let mut env = JsEnvironment::new(Rc::clone(&doc));

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        let (action, query) = env.take_pending_navigation().expect("GET form submit should set a pending navigation");
        assert_eq!(action, "/search");
        assert_eq!(query, "q=hello+world", "unchecked checkbox must be excluded, named field must be present and encoded");
    }

    #[test]
    fn submitting_a_form_with_no_action_reports_an_empty_action() {
        let doc = make_doc_with_get_form("");
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.dispatch_click(&full_hit_path_for(&doc, "go"));
        let (action, _query) = env.take_pending_navigation().expect("should still navigate, to the current page");
        assert_eq!(action, "");
    }

    #[test]
    fn prevent_default_on_submit_suppresses_classic_navigation() {
        let doc = make_doc_with_get_form("/search");
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('f').addEventListener('submit', (e) => e.preventDefault());").unwrap();

        env.dispatch_click(&full_hit_path_for(&doc, "go"));

        assert_eq!(env.take_pending_navigation(), None);
    }

    #[test]
    fn an_invalid_form_never_sets_pending_navigation() {
        let doc = make_doc_with_form_field(true, None); // required, empty
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.dispatch_click(&full_hit_path_for(&doc, "go"));
        assert_eq!(env.take_pending_navigation(), None, "an invalid form must not navigate either, same as it must not fire submit");
    }

    // ---- Implicit submission: Enter in a text field (M16's gap) ----

    fn form_page(body: &str) -> (Rc<RefCell<Document>>, JsEnvironment) {
        let root = html_parser::parse_document(&format!("<html><body>{body}</body></html>")).unwrap();
        let doc = Rc::new(RefCell::new(Document::new(root)));
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.log = []; document.getElementById('f').addEventListener('submit', () => log.push('submit'));").unwrap();
        (doc, env)
    }

    fn press_enter_in(env: &mut JsEnvironment, field: &str) -> String {
        env.set_focus(Some(field.to_string()));
        env.dispatch_keydown("Enter");
        env.run_script("log.join(',')").unwrap()
    }

    #[test]
    fn enter_in_the_only_text_field_of_a_form_without_a_button_submits_it() {
        let (_doc, mut env) = form_page("<form id=f action=/s><input id=q name=q value=cats></form>");
        assert_eq!(press_enter_in(&mut env, "q"), "submit");
        assert_eq!(env.take_pending_navigation(), Some(("/s".to_string(), "q=cats".to_string())));
    }

    #[test]
    fn enter_clicks_the_default_button_which_submits() {
        let (_doc, mut env) = form_page("<form id=f action=/s><input id=q name=q><input id=r name=r><button type=button id=other>x</button><button id=go>Go</button></form>");
        env.run_script("document.getElementById('go').addEventListener('click', () => log.push('click'));").unwrap();
        assert_eq!(press_enter_in(&mut env, "q"), "click,submit", "the first submit button is clicked; type=button is not one");
        assert!(env.take_pending_navigation().is_some());
    }

    #[test]
    fn enter_does_not_submit_when_the_default_button_is_disabled_or_two_fields_block_it() {
        let (_doc, mut env) = form_page("<form id=f><input id=q><button id=go disabled>Go</button></form>");
        assert_eq!(press_enter_in(&mut env, "q"), "");
        let (_doc, mut env) = form_page("<form id=f><input id=q><input id=r type=email></form>");
        assert_eq!(press_enter_in(&mut env, "q"), "", "two blocking fields and no button: no implicit submission");
        assert_eq!(env.take_pending_navigation(), None);
    }

    #[test]
    fn enter_on_a_focused_button_clicks_it_unless_disabled() {
        let (_doc, mut env) = form_page("<form id=f><input id=a><input id=b><button id=go>Go</button><button id=off disabled>x</button></form>");
        env.run_script("document.getElementById('go').addEventListener('click', () => log.push('click'));").unwrap();
        assert_eq!(press_enter_in(&mut env, "go"), "click,submit");
        assert_eq!(press_enter_in(&mut env, "off"), "click,submit", "a disabled button does nothing");
    }

    #[test]
    fn enter_does_not_submit_from_a_textarea_a_checkbox_or_when_keydown_is_prevented() {
        let (_doc, mut env) = form_page("<form id=f><textarea id=t></textarea><input id=c type=checkbox></form>");
        assert_eq!(press_enter_in(&mut env, "t"), "");
        assert_eq!(press_enter_in(&mut env, "c"), "");
        let (_doc, mut env) = form_page("<form id=f><input id=q></form>");
        env.run_script("document.addEventListener('keydown', (e) => e.preventDefault());").unwrap();
        assert_eq!(press_enter_in(&mut env, "q"), "");
    }

    // ---- Milestone 13: mutation dirty-flag ----

    #[test]
    fn mutating_dom_sets_the_dirty_flag() {
        let mut env = JsEnvironment::new(make_doc());
        assert!(!env.take_dirty(), "should start clean");
        env.run_script("document.getElementById('headline').textContent = 'x'").unwrap();
        assert!(env.take_dirty(), "mutation should have set dirty");
        assert!(!env.take_dirty(), "take_dirty should consume/reset the flag");
    }

    #[test]
    fn reading_dom_does_not_set_the_dirty_flag() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("document.getElementById('headline').textContent").unwrap();
        assert!(!env.take_dirty(), "a read should never mark the page dirty");
    }

    #[test]
    fn style_mutation_also_sets_dirty() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("document.getElementById('headline').style.color = 'blue'").unwrap();
        assert!(env.take_dirty());
    }

    // ---- Milestone 12: localStorage/sessionStorage ----

    #[test]
    fn local_storage_set_and_get_item_roundtrip() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("localStorage.setItem('key', 'value')").unwrap();
        let result = env.run_script("localStorage.getItem('key')").unwrap();
        assert_eq!(result, "value");
    }

    #[test]
    fn local_storage_missing_key_returns_null() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("localStorage.getItem('missing') === null").unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn local_storage_remove_item_removes_it() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("localStorage.setItem('a', '1')").unwrap();
        env.run_script("localStorage.removeItem('a')").unwrap();
        let result = env.run_script("localStorage.getItem('a') === null").unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn local_storage_clear_empties_the_store() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("localStorage.setItem('a', '1'); localStorage.setItem('b', '2'); localStorage.clear();")
            .unwrap();
        let result = env.run_script("localStorage.getItem('a') === null && localStorage.getItem('b') === null").unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn session_storage_is_independent_from_local_storage() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("localStorage.setItem('shared', 'local')").unwrap();
        env.run_script("sessionStorage.setItem('shared', 'session')").unwrap();
        assert_eq!(env.run_script("localStorage.getItem('shared')").unwrap(), "local");
        assert_eq!(env.run_script("sessionStorage.getItem('shared')").unwrap(), "session");
    }

    // ---- Milestone 12: fetch() ----

    /// Polls `poll_pending_fetches` in a loop with a bounded timeout —
    /// what `app`'s real tick loop does every frame, condensed for a
    /// test. `fetch()` is now genuinely asynchronous (see
    /// `network::NetworkContext::fetch_async`), so a test that dispatches
    /// one can no longer expect the result to be ready immediately
    /// after a single `run_script` call — this is that real waiting,
    /// not an artifact worth hiding.
    fn poll_until_no_pending_fetches(env: &mut JsEnvironment) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while env.has_pending_fetches() {
            assert!(std::time::Instant::now() < deadline, "fetch never settled within the timeout");
            std::thread::sleep(std::time::Duration::from_millis(5));
            env.poll_pending_fetches();
        }
    }

    #[test]
    fn fetch_against_a_real_local_server_resolves_with_the_body() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).ok();
                loop {
                    let mut l = String::new();
                    reader.read_line(&mut l).ok();
                    if l.trim().is_empty() {
                        break;
                    }
                }
                let body = b"hello from a real socket";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                    .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        // The page is on the server's own origin: a same-origin read.
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("http://127.0.0.1:{port}/page.html")));
        env.run_script(&format!(
            "globalThis.result = null; \
             fetch('http://127.0.0.1:{port}').then(r => r.text()).then(t => {{ globalThis.result = t; }});"
        ))
        .unwrap();

        assert!(env.has_pending_fetches(), "fetch() should be genuinely async — not settled immediately");
        poll_until_no_pending_fetches(&mut env);

        let result = env.run_script("globalThis.result").unwrap();
        assert_eq!(result, "hello from a real socket");
    }

    /// Spawns a one-shot mock HTTP server (same shape as the test
    /// above) that always replies 200 with `body`, plus whatever extra
    /// response headers `extra_headers` specifies (used here to test
    /// `Access-Control-Allow-Origin`). Returns the port.
    fn spawn_mock_server_with_headers(body: &'static str, extra_headers: &'static str) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).ok();
                loop {
                    let mut l = String::new();
                    reader.read_line(&mut l).ok();
                    if l.trim().is_empty() {
                        break;
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body.as_bytes()).unwrap();
            }
        });
        port
    }

    /// Security regression: a cross-origin `fetch()` from a page with a
    /// REAL origin, against a server that does NOT opt in via CORS,
    /// must be BLOCKED — the promise rejects, script never sees the
    /// response body. Without `is_cross_origin_response_blocked`, any
    /// page could read the full response of a request to any other
    /// origin, and since cookies are attached per target-domain
    /// regardless of which page initiated the request, that includes
    /// another site's authenticated content.
    #[test]
    fn cross_origin_fetch_without_cors_header_is_blocked() {
        let port = spawn_mock_server_with_headers("secret cross-origin body", "");
        // A different scheme+host than `http://127.0.0.1:<port>` —
        // guaranteed cross-origin.
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
        env.run_script(&format!(
            "globalThis.result = 'not settled'; \
             fetch('http://127.0.0.1:{port}').then(r => r.text()).then(t => {{ globalThis.result = 'resolved:' + t; }}).catch(e => {{ globalThis.result = 'rejected'; }});"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut env);
        assert_eq!(env.run_script("globalThis.result").unwrap(), "rejected", "a cross-origin response with no CORS header must never reach script");
    }

    /// The other half: a cross-origin response that DOES opt in via a
    /// matching `Access-Control-Allow-Origin` must still be readable —
    /// this is real (if minimal) CORS support, not "block everything
    /// cross-origin".
    #[test]
    fn cross_origin_fetch_with_matching_acao_header_is_allowed() {
        let port = spawn_mock_server_with_headers("public cross-origin body", "Access-Control-Allow-Origin: https://example.test\r\n");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
        env.run_script(&format!(
            "globalThis.result = 'not settled'; \
             fetch('http://127.0.0.1:{port}').then(r => r.text()).then(t => {{ globalThis.result = t; }}).catch(e => {{ globalThis.result = 'rejected'; }});"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut env);
        assert_eq!(env.run_script("globalThis.result").unwrap(), "public cross-origin body");
    }

    /// A same-origin `fetch()` must never be blocked, CORS header or
    /// not — the same-origin policy only restricts CROSS-origin reads.
    #[test]
    fn same_origin_fetch_is_never_blocked() {
        let port = spawn_mock_server_with_headers("same-origin body", "");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("http://127.0.0.1:{port}/page.html")));
        env.run_script(&format!(
            "globalThis.result = 'not settled'; \
             fetch('http://127.0.0.1:{port}/data').then(r => r.text()).then(t => {{ globalThis.result = t; }}).catch(e => {{ globalThis.result = 'rejected'; }});"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut env);
        assert_eq!(env.run_script("globalThis.result").unwrap(), "same-origin body");
    }

    #[test]
    fn fetch_does_not_block_subsequent_script_execution() {
        // The real proof of concurrency at the JS level: dispatch a
        // fetch against a server that deliberately sleeps 200ms, then
        // immediately run MORE script afterward in the same
        // run_script call. If fetch() blocked, this whole call would
        // take 200ms+; instead it returns essentially immediately,
        // proving the JS engine's single thread was never stuck
        // waiting on the network thread.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).ok();
                loop {
                    let mut l = String::new();
                    reader.read_line(&mut l).ok();
                    if l.trim().is_empty() {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            }
        });

        let mut env = JsEnvironment::new(make_doc());
        let start = std::time::Instant::now();
        env.run_script(&format!(
            "fetch('http://127.0.0.1:{port}'); globalThis.reachedHere = true;"
        ))
        .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(env.run_script("globalThis.reachedHere").unwrap(), "true");
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "run_script took {elapsed:?} — fetch() appears to have blocked on the server's 200ms delay"
        );

        poll_until_no_pending_fetches(&mut env); // drain cleanly so the test's background thread can finish
    }

    #[test]
    fn fetch_against_an_unroutable_url_rejects_and_catch_runs() {
        let mut env = JsEnvironment::new(make_doc());
        // No real network access in this sandbox — connection-refused
        // against an unbound local port is a real, fast, deterministic
        // failure (same technique `network`'s own tests use).
        env.run_script(
            "globalThis.caught = false; \
             fetch('http://127.0.0.1:1').catch(() => { globalThis.caught = true; });",
        )
        .expect("fetch() itself must not throw synchronously — it should return a Promise");

        poll_until_no_pending_fetches(&mut env);

        let result = env.run_script("globalThis.caught").unwrap();
        assert_eq!(result, "true", ".catch() should have run for a rejected fetch");
    }

    /// M40's gap: an operation that returns a promise never throws (WebIDL
    /// §3.2.1) — a missing or ill-typed argument rejects the promise with
    /// the `TypeError` instead.
    #[test]
    fn a_promise_returning_operation_rejects_instead_of_throwing() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script(
            "globalThis.out = []; \
             const p1 = fetch('http://127.0.0.1:1/', {credentials: 'bogus'}); \
             const p2 = fetch(); \
             out.push(p1 instanceof Promise, p2 instanceof Promise); \
             p1.catch(e => out.push(e.name)); p2.catch(e => out.push(e.name));",
        )
        .expect("neither call may throw");
        env.run_script("0").unwrap(); // let the reactions run
        assert_eq!(env.run_script("out.join(',')").unwrap(), "true,true,TypeError,TypeError");
    }

    // ---- Milestone 14: requestAnimationFrame ----

    #[test]
    fn raf_callback_does_not_run_until_tick_is_called() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("window_ran = false; requestAnimationFrame(() => { window_ran = true; });").ok();
        // Boa's globalThis doesn't need "window." prefix for a plain
        // global var — check via a fresh eval.
        let ran_before = env.run_script("typeof window_ran !== 'undefined' ? window_ran : false").unwrap();
        assert_eq!(ran_before, "false");
    }

    #[test]
    fn raf_callback_runs_on_tick_and_receives_a_timestamp() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("globalThis.sawTimestamp = -1; requestAnimationFrame((t) => { globalThis.sawTimestamp = t; });")
            .unwrap();
        env.run_animation_frame_callbacks(123.5);
        let result = env.run_script("globalThis.sawTimestamp").unwrap();
        // Timestamps are coarsened to whole milliseconds (hardening pass) —
        // this used to pin the full-precision 123.5.
        assert_eq!(result, "123");
    }

    #[test]
    fn raf_callback_mutating_dom_sets_dirty_flag() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "requestAnimationFrame(() => { document.getElementById('headline').textContent = 'animated'; });",
        )
        .unwrap();
        assert!(!env.take_dirty(), "registering a callback alone shouldn't dirty the page");
        env.run_animation_frame_callbacks(16.0);
        assert!(env.take_dirty(), "the callback's mutation should have dirtied the page");
        assert_eq!(doc.borrow().find_by_id("headline").unwrap().text_content(), "animated");
    }

    #[test]
    fn raf_callbacks_are_cleared_after_running_once() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("globalThis.n = 0; requestAnimationFrame(() => { globalThis.n += 1; });").unwrap();
        env.run_animation_frame_callbacks(0.0);
        env.run_animation_frame_callbacks(16.0); // no new callback registered — should NOT run again
        let result = env.run_script("globalThis.n").unwrap();
        assert_eq!(result, "1");
    }

    #[test]
    fn has_pending_animation_frame_callbacks_reflects_registration() {
        let mut env = JsEnvironment::new(make_doc());
        assert!(!env.has_pending_animation_frame_callbacks());
        env.run_script("requestAnimationFrame(() => {});").unwrap();
        assert!(env.has_pending_animation_frame_callbacks());
        env.run_animation_frame_callbacks(0.0);
        assert!(!env.has_pending_animation_frame_callbacks());
    }

    #[test]
    fn a_raf_callback_that_registers_another_raf_runs_it_next_tick_not_this_one() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script(
            "globalThis.n = 0; \
             requestAnimationFrame(function again() { \
                 globalThis.n += 1; \
                 if (globalThis.n < 3) requestAnimationFrame(again); \
             });",
        )
        .unwrap();

        env.run_animation_frame_callbacks(0.0);
        assert_eq!(env.run_script("globalThis.n").unwrap(), "1");
        assert!(env.has_pending_animation_frame_callbacks(), "should have re-registered itself");

        env.run_animation_frame_callbacks(16.0);
        assert_eq!(env.run_script("globalThis.n").unwrap(), "2");

        env.run_animation_frame_callbacks(32.0);
        assert_eq!(env.run_script("globalThis.n").unwrap(), "3");
        assert!(!env.has_pending_animation_frame_callbacks(), "n==3 should have stopped re-registering");
    }

    #[test]
    fn style_transform_is_settable_and_readable_from_js() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("document.getElementById('headline').style.transform = 'translate(5px, 0px)'").unwrap();
        let style = doc.borrow().find_by_id("headline").unwrap().attr("style").unwrap().to_string();
        assert!(style.contains("transform:translate(5px, 0px)"));
    }

    #[test]
    fn raf_driven_transform_animation_updates_each_tick() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "function animate(t) { \
                 document.getElementById('headline').style.transform = 'translate(' + t + 'px, 0px)'; \
                 requestAnimationFrame(animate); \
             } \
             requestAnimationFrame(animate);",
        )
        .unwrap();

        env.run_animation_frame_callbacks(1.0);
        let style1 = doc.borrow().find_by_id("headline").unwrap().attr("style").unwrap().to_string();
        assert!(style1.contains("translate(1px"));

        env.run_animation_frame_callbacks(2.0);
        let style2 = doc.borrow().find_by_id("headline").unwrap().attr("style").unwrap().to_string();
        assert!(style2.contains("translate(2px"));
        assert!(env.has_pending_animation_frame_callbacks(), "animate() should keep re-registering itself");
    }

    // ---- Document ordering: multiple scripts sharing state ----

    #[test]
    fn multiple_script_runs_share_the_same_global_state() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("globalThis.counter = 1;").unwrap();
        env.run_script("globalThis.counter += 1;").unwrap();
        let result = env.run_script("globalThis.counter").unwrap();
        assert_eq!(result, "2");
    }

    // ---- Milestone 15: event dispatch ----

    fn element_key_for(doc: &Rc<RefCell<Document>>, id: &str) -> dom::NodeId {
        let doc = doc.borrow();
        (doc.find_by_id(id).unwrap()).node_id()
    }

    #[test]
    fn click_fires_the_registered_listener() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.clicked = false; \
             document.getElementById('headline').addEventListener('click', () => { globalThis.clicked = true; });",
        )
        .unwrap();

        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]);

        assert_eq!(env.run_script("globalThis.clicked").unwrap(), "true");
    }

    #[test]
    fn click_on_unregistered_element_does_not_panic() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]); // no listeners registered — must not panic
    }

    #[test]
    fn click_bubbles_to_ancestor_listeners() {
        let mut root = dom::Element::new("div");
        root.attributes.push(("id".into(), "outer".into()));
        let mut inner = dom::Element::new("span");
        inner.attributes.push(("id".into(), "inner".into()));
        root.children.push(dom::Node::Element(inner));
        let doc = Rc::new(RefCell::new(Document::new(root)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.order = []; \
             document.getElementById('inner').addEventListener('click', () => globalThis.order.push('inner')); \
             document.getElementById('outer').addEventListener('click', () => globalThis.order.push('outer'));",
        )
        .unwrap();

        let inner_key = element_key_for(&doc, "inner");
        let outer_key = element_key_for(&doc, "outer");
        // Bubble order: target (inner) first, ancestor (outer) after —
        // exactly what layout::hit_test_path produces.
        env.dispatch_click(&[inner_key, outer_key]);

        assert_eq!(env.run_script("globalThis.order.join(',')").unwrap(), "inner,outer");
    }

    #[test]
    fn click_also_fires_document_level_listeners() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.docClicked = false; \
             document.addEventListener('click', () => { globalThis.docClicked = true; });",
        )
        .unwrap();

        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]);

        assert_eq!(env.run_script("globalThis.docClicked").unwrap(), "true");
    }

    #[test]
    fn click_sets_focus_to_the_target_element() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        assert_eq!(env.focused_element_id(), None);

        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]);

        assert_eq!(env.focused_element_id(), Some("headline".to_string()));
    }

    #[test]
    fn click_mutating_dom_is_visible_through_the_shared_tree() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "document.getElementById('headline').addEventListener('click', () => { \
                 document.getElementById('headline').textContent = 'clicked!'; \
             });",
        )
        .unwrap();

        let key = element_key_for(&doc, "headline");
        env.dispatch_click(&[key]);

        assert_eq!(doc.borrow().find_by_id("headline").unwrap().text_content(), "clicked!");
        assert!(env.take_dirty(), "a DOM mutation from an event handler should set dirty, same as any other mutation");
    }

    #[test]
    fn keydown_fires_on_the_focused_element() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.lastKey = ''; \
             document.getElementById('headline').addEventListener('keydown', (e) => { globalThis.lastKey = e.key; });",
        )
        .unwrap();

        env.set_focus(Some("headline".to_string()));
        env.dispatch_keydown("a");

        assert_eq!(env.run_script("globalThis.lastKey").unwrap(), "a");
    }

    #[test]
    fn keydown_with_no_focus_does_not_panic_and_still_reaches_document_listeners() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.sawIt = false; \
             document.addEventListener('keydown', () => { globalThis.sawIt = true; });",
        )
        .unwrap();

        assert_eq!(env.focused_element_id(), None);
        env.dispatch_keydown("Enter"); // no focused element — must not panic

        assert_eq!(env.run_script("globalThis.sawIt").unwrap(), "true");
    }

    #[test]
    fn keydown_does_not_fire_on_a_non_focused_element() {
        let mut root = dom::Element::new("div");
        let mut a = dom::Element::new("input");
        a.attributes.push(("id".into(), "a".into()));
        let mut b = dom::Element::new("input");
        b.attributes.push(("id".into(), "b".into()));
        root.children.push(dom::Node::Element(a));
        root.children.push(dom::Node::Element(b));
        let doc = Rc::new(RefCell::new(Document::new(root)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.aFired = false; \
             document.getElementById('a').addEventListener('keydown', () => { globalThis.aFired = true; });",
        )
        .unwrap();

        env.set_focus(Some("b".to_string()));
        env.dispatch_keydown("x");

        assert_eq!(env.run_script("globalThis.aFired").unwrap(), "false");
    }

    #[test]
    fn scroll_fires_document_level_listeners() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.scrolled = false; \
             document.addEventListener('scroll', () => { globalThis.scrolled = true; });",
        )
        .unwrap();

        env.dispatch_scroll();

        assert_eq!(env.run_script("globalThis.scrolled").unwrap(), "true");
    }

    #[test]
    fn set_focus_to_none_clears_focus() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("headline".to_string()));
        assert_eq!(env.focused_element_id(), Some("headline".to_string()));
        env.set_focus(None);
        assert_eq!(env.focused_element_id(), None);
    }

    #[test]
    fn rapid_repeated_clicks_do_not_crash_or_desync() {
        // Milestone 15 acceptance criterion: spam-clicking must not
        // crash or leave listener/dirty state inconsistent.
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.n = 0; \
             document.getElementById('headline').addEventListener('click', () => { globalThis.n += 1; });",
        )
        .unwrap();

        let key = element_key_for(&doc, "headline");
        for _ in 0..500 {
            env.dispatch_click(&[key]);
        }

        assert_eq!(env.run_script("globalThis.n").unwrap(), "500");
    }

    // ---- Milestone 16: text input editing ----

    fn make_input_doc() -> Rc<RefCell<Document>> {
        let mut root = dom::Element::new("div");
        let mut input = dom::Element::new("input");
        input.attributes.push(("id".into(), "field".into()));
        input.attributes.push(("type".into(), "text".into()));
        root.children.push(dom::Node::Element(input));
        Rc::new(RefCell::new(Document::new(root)))
    }

    #[test]
    fn typing_a_character_inserts_it_into_the_focused_input() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));

        env.dispatch_keydown("h");
        env.dispatch_keydown("i");

        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), Some("hi"));
        assert_eq!(env.cursor_position("field"), 2);
    }

    #[test]
    fn typing_sets_dirty_for_redraw() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.take_dirty();
        env.dispatch_keydown("x");
        assert!(env.take_dirty());
    }

    #[test]
    fn backspace_deletes_the_character_before_the_cursor() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("a");
        env.dispatch_keydown("b");
        env.dispatch_keydown("c");
        env.dispatch_keydown("Backspace");

        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), Some("ab"));
        assert_eq!(env.cursor_position("field"), 2);
    }

    #[test]
    fn backspace_at_start_of_input_is_a_no_op_not_a_panic() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("Backspace"); // empty input, cursor at 0
        // Still empty — and since M31 not even rewritten: a key that changes
        // no text makes no DOM mutation (so no restyle or relayout).
        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value").unwrap_or(""), "");
        assert!(doc.borrow().pending_mutations().is_empty(), "no mutation for a no-op key");
    }

    #[test]
    fn arrow_keys_move_the_cursor_without_changing_text() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("a");
        env.dispatch_keydown("b");
        env.dispatch_keydown("c"); // "abc", cursor at 3

        env.dispatch_keydown("ArrowLeft");
        assert_eq!(env.cursor_position("field"), 2);
        env.dispatch_keydown("ArrowLeft");
        assert_eq!(env.cursor_position("field"), 1);
        env.dispatch_keydown("ArrowRight");
        assert_eq!(env.cursor_position("field"), 2);

        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), Some("abc"));
    }

    #[test]
    fn typing_inserts_at_the_cursor_not_always_at_the_end() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("a");
        env.dispatch_keydown("c"); // "ac"
        env.dispatch_keydown("ArrowLeft"); // cursor between a and c
        env.dispatch_keydown("b"); // insert b at cursor -> "abc"

        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), Some("abc"));
    }

    #[test]
    fn home_and_end_move_cursor_to_boundaries() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("a");
        env.dispatch_keydown("b");
        env.dispatch_keydown("c");

        env.dispatch_keydown("Home");
        assert_eq!(env.cursor_position("field"), 0);
        env.dispatch_keydown("End");
        assert_eq!(env.cursor_position("field"), 3);
    }

    #[test]
    fn typing_with_no_focus_does_not_panic() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.dispatch_keydown("x"); // nothing focused — must not panic, and must not create a value attribute
        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), None);
    }

    #[test]
    fn typing_while_a_non_input_is_focused_does_not_edit_anything() {
        let doc = make_doc(); // headline div, not an input
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("headline".to_string()));
        env.dispatch_keydown("x"); // should not attempt to treat headline as editable
        assert_eq!(doc.borrow().find_by_id("headline").unwrap().text_content(), "original");
    }

    // ---- Milestone 23: contenteditable ----

    fn make_contenteditable_doc() -> Rc<RefCell<Document>> {
        let mut root = dom::Element::new("body");
        let mut note = dom::Element::new("div");
        note.attributes.push(("id".into(), "note".into()));
        note.attributes.push(("contenteditable".into(), "true".into()));
        note.set_text_content("hi");
        root.children.push(dom::Node::Element(note));
        Rc::new(RefCell::new(Document::new(root)))
    }

    #[test]
    fn typing_into_a_contenteditable_element_edits_its_text_content() {
        let doc = make_contenteditable_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("note".to_string()));
        env.set_cursor_position("note", 2); // end of "hi"
        env.dispatch_keydown("!");

        assert_eq!(doc.borrow().find_by_id("note").unwrap().text_content(), "hi!");
        assert_eq!(env.cursor_position("note"), 3);
    }

    #[test]
    fn backspace_in_a_contenteditable_element_deletes_from_its_text_content() {
        let doc = make_contenteditable_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("note".to_string()));
        env.set_cursor_position("note", 2); // end of "hi"
        env.dispatch_keydown("Backspace");

        assert_eq!(doc.borrow().find_by_id("note").unwrap().text_content(), "h");
    }

    #[test]
    fn contenteditable_false_is_not_editable() {
        let mut root = dom::Element::new("body");
        let mut note = dom::Element::new("div");
        note.attributes.push(("id".into(), "note".into()));
        note.attributes.push(("contenteditable".into(), "false".into()));
        note.set_text_content("hi");
        root.children.push(dom::Node::Element(note));
        let doc = Rc::new(RefCell::new(Document::new(root)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("note".to_string()));
        env.dispatch_keydown("x");

        assert_eq!(doc.borrow().find_by_id("note").unwrap().text_content(), "hi");
    }

    #[test]
    fn typing_into_contenteditable_sets_dirty_for_redraw() {
        let doc = make_contenteditable_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("note".to_string()));
        env.take_dirty();
        env.dispatch_keydown("x");
        assert!(env.take_dirty());
    }

    #[test]
    fn text_input_editing_does_not_prevent_keydown_listeners_from_firing() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.sawKey = null; \
             document.getElementById('field').addEventListener('keydown', (e) => { globalThis.sawKey = e.key; });",
        )
        .unwrap();
        env.set_focus(Some("field".to_string()));
        env.dispatch_keydown("q");

        assert_eq!(doc.borrow().find_by_id("field").unwrap().attr("value"), Some("q"));
        assert_eq!(env.run_script("globalThis.sawKey").unwrap(), "q");
    }

    // ---- Milestone 16: tab order ----

    #[test]
    fn tab_moves_focus_to_the_next_element_in_order() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let order = vec!["field".to_string(), "button1".to_string(), "button2".to_string()];

        env.focus_next_in_tab_order(&order);
        assert_eq!(env.focused_element_id(), Some("field".to_string()));
        env.focus_next_in_tab_order(&order);
        assert_eq!(env.focused_element_id(), Some("button1".to_string()));
        env.focus_next_in_tab_order(&order);
        assert_eq!(env.focused_element_id(), Some("button2".to_string()));
    }

    #[test]
    fn tab_wraps_around_after_the_last_element() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let order = vec!["a".to_string(), "b".to_string()];
        env.set_focus(Some("b".to_string()));
        env.focus_next_in_tab_order(&order);
        assert_eq!(env.focused_element_id(), Some("a".to_string()));
    }

    #[test]
    fn tab_with_empty_order_does_not_panic() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.focus_next_in_tab_order(&[]);
        assert_eq!(env.focused_element_id(), None);
    }

    #[test]
    fn tab_from_unfocused_state_focuses_the_first_element() {
        let doc = make_input_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let order = vec!["first".to_string(), "second".to_string()];
        env.focus_next_in_tab_order(&order);
        assert_eq!(env.focused_element_id(), Some("first".to_string()));
    }

    // ---- Milestone 16: form submission ----

    fn make_form_doc() -> Rc<RefCell<Document>> {
        let mut form = dom::Element::new("form");
        form.attributes.push(("id".into(), "myform".into()));
        let mut input = dom::Element::new("input");
        input.attributes.push(("id".into(), "field".into()));
        let mut button = dom::Element::new("button");
        button.attributes.push(("id".into(), "submitBtn".into()));
        form.children.push(dom::Node::Element(input));
        form.children.push(dom::Node::Element(button));
        Rc::new(RefCell::new(Document::new(form)))
    }

    #[test]
    fn clicking_a_button_fires_submit_on_the_enclosing_form() {
        let doc = make_form_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.submitted = false; \
             document.getElementById('myform').addEventListener('submit', () => { globalThis.submitted = true; });",
        )
        .unwrap();

        let button_key = element_key_for(&doc, "submitBtn");
        let form_key = element_key_for(&doc, "myform");
        // Bubble path: button (target) then its form ancestor.
        env.dispatch_click(&[button_key, form_key]);

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "true");
    }

    #[test]
    fn clicking_a_non_button_does_not_fire_submit() {
        let doc = make_form_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.submitted = false; \
             document.getElementById('myform').addEventListener('submit', () => { globalThis.submitted = true; });",
        )
        .unwrap();

        let input_key = element_key_for(&doc, "field");
        let form_key = element_key_for(&doc, "myform");
        env.dispatch_click(&[input_key, form_key]); // clicked the input, not the button

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "false");
    }

    #[test]
    fn input_type_submit_also_fires_form_submit() {
        let mut form = dom::Element::new("form");
        form.attributes.push(("id".into(), "f2".into()));
        let mut submit_input = dom::Element::new("input");
        submit_input.attributes.push(("id".into(), "go".into()));
        submit_input.attributes.push(("type".into(), "submit".into()));
        form.children.push(dom::Node::Element(submit_input));
        let doc = Rc::new(RefCell::new(Document::new(form)));

        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script(
            "globalThis.submitted = false; \
             document.getElementById('f2').addEventListener('submit', () => { globalThis.submitted = true; });",
        )
        .unwrap();

        let go_key = element_key_for(&doc, "go");
        let form_key = element_key_for(&doc, "f2");
        env.dispatch_click(&[go_key, form_key]);

        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "true");
    }

    #[test]
    fn submit_with_no_enclosing_form_does_not_panic() {
        let mut button = dom::Element::new("button");
        button.attributes.push(("id".into(), "orphan".into()));
        let doc = Rc::new(RefCell::new(Document::new(button)));
        let mut env = JsEnvironment::new(Rc::clone(&doc));

        let key = element_key_for(&doc, "orphan");
        env.dispatch_click(&[key]); // no form ancestor at all — must not panic
    }

    // ---- Phase F: richer Response, XMLHttpRequest ----

    /// A tiny loop-accept HTTP server that replies with a fixed status
    /// line + headers + body to every request. Returns its base URL.
    fn spawn_fixed_server(status_line: &'static str, headers: &'static str, body: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut l = String::new();
                reader.read_line(&mut l).ok();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).ok();
                    if h.trim().is_empty() {
                        break;
                    }
                }
                let _ = write!(
                    stream,
                    "{status_line}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn fetch_response_exposes_ok_status_text_url_and_headers() {
        let base = spawn_fixed_server("HTTP/1.1 404 Not Found", "X-Trace: abc\r\n", "missing");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("{base}/page.html")));
        env.run_script(&format!(
            "globalThis.o=null; fetch('{base}/thing').then(r => {{ \
               globalThis.o = {{ ok: r.ok, status: r.status, st: r.statusText, url: r.url, trace: r.headers.get('x-trace'), has: r.headers.has('nope') }}; \
             }});"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut env);

        assert_eq!(env.run_script("globalThis.o.ok").unwrap(), "false");
        assert_eq!(env.run_script("globalThis.o.status").unwrap(), "404");
        assert_eq!(env.run_script("globalThis.o.st").unwrap(), "Not Found");
        assert_eq!(env.run_script("globalThis.o.trace").unwrap(), "abc");
        assert_eq!(env.run_script("globalThis.o.has").unwrap(), "false");
        assert!(env.run_script("globalThis.o.url").unwrap().contains("/thing"));
    }

    #[test]
    fn fetch_response_json_parses_the_body() {
        let base = spawn_fixed_server("HTTP/1.1 200 OK", "Content-Type: application/json\r\n", "{\"a\":1,\"b\":[2,3]}");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("{base}/page.html")));
        env.run_script(&format!(
            "globalThis.v=null; fetch('{base}/d.json').then(r => r.json()).then(j => {{ globalThis.v = j.a + j.b[1]; }});"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut env);
        assert_eq!(env.run_script("globalThis.v").unwrap(), "4");
    }

    /// Performance fix regression: `send()` must NOT block — it should
    /// return before the response arrives (proven here by checking
    /// `readyState`/`loaded` are still at their pre-response values
    /// immediately after `send()`, the same non-blocking proof
    /// `fetch_does_not_block_subsequent_script_execution` uses), and
    /// only settle once polled, exactly like `fetch()`.
    #[test]
    fn xmlhttprequest_send_does_not_block_and_populates_response_and_fires_onload_once_polled() {
        let base = spawn_fixed_server("HTTP/1.1 200 OK", "X-K: v\r\n", "xhr body");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("{base}/page.html")));
        env.run_script(&format!(
            "globalThis.loaded = false; \
             globalThis.x = new XMLHttpRequest(); \
             x.onload = function() {{ globalThis.loaded = true; }}; \
             x.open('GET', '{base}/x'); \
             x.setRequestHeader('X-Client', 'test'); \
             x.send();"
        ))
        .unwrap();

        // send() must have returned WITHOUT the response already
        // settled — the whole point of this fix.
        assert_eq!(env.run_script("globalThis.loaded").unwrap(), "false", "send() must not block; onload can't have fired yet");
        assert!(env.has_pending_fetches(), "send() should be genuinely async, tracked as a pending fetch");

        poll_until_no_pending_fetches(&mut env);

        env.run_script(
            "globalThis.rs = x.readyState; globalThis.st = x.status; globalThis.body = x.responseText; \
             globalThis.hk = x.getResponseHeader('x-k');",
        )
        .unwrap();
        assert_eq!(env.run_script("globalThis.loaded").unwrap(), "true");
        assert_eq!(env.run_script("globalThis.rs").unwrap(), "4");
        assert_eq!(env.run_script("globalThis.st").unwrap(), "200");
        assert_eq!(env.run_script("globalThis.body").unwrap(), "xhr body");
        assert_eq!(env.run_script("globalThis.hk").unwrap(), "v");
    }

    /// Security regression: the same same-origin enforcement `fetch()`
    /// gets (see `cross_origin_fetch_without_cors_header_is_blocked`)
    /// must also apply to `XMLHttpRequest` — a cross-origin response
    /// with no matching `Access-Control-Allow-Origin` must come back as
    /// a network error (`status` 0, empty `responseText`), never the
    /// real body/headers.
    #[test]
    fn xhr_cross_origin_without_cors_header_is_blocked() {
        let base = spawn_fixed_server("HTTP/1.1 200 OK", "", "secret xhr body");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
        env.run_script(&format!("globalThis.x = new XMLHttpRequest(); x.open('GET', '{base}/x'); x.send();")).unwrap();
        poll_until_no_pending_fetches(&mut env);
        env.run_script("globalThis.st = x.status; globalThis.body = x.responseText;").unwrap();
        assert_eq!(env.run_script("globalThis.st").unwrap(), "0", "a blocked cross-origin XHR must report status 0, like a network error");
        let body = env.run_script("globalThis.body").unwrap();
        assert!(!body.contains("secret xhr body"), "the real cross-origin body must never reach script, got: {body}");
    }

    #[test]
    fn xhr_cross_origin_with_matching_acao_header_is_allowed() {
        let base = spawn_fixed_server("HTTP/1.1 200 OK", "Access-Control-Allow-Origin: https://example.test\r\n", "public xhr body");
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
        env.run_script(&format!("globalThis.x = new XMLHttpRequest(); x.open('GET', '{base}/x'); x.send();")).unwrap();
        poll_until_no_pending_fetches(&mut env);
        env.run_script("globalThis.st = x.status; globalThis.body = x.responseText;").unwrap();
        assert_eq!(env.run_script("globalThis.st").unwrap(), "200");
        assert_eq!(env.run_script("globalThis.body").unwrap(), "public xhr body");
    }

    #[test]
    fn xmlhttprequest_on_a_dead_port_reports_status_zero_without_panicking() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("globalThis.x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:1/'); x.send();").unwrap();
        poll_until_no_pending_fetches(&mut env);
        env.run_script("globalThis.s = x.status;").unwrap();
        assert_eq!(env.run_script("globalThis.s").unwrap(), "0");
    }

    // ---- Milestone 26: fetch()/XHR share the profile's network context ----

    /// A reusable (multi-connection) mock server: `/set` replies with a
    /// `Set-Cookie`; `/echo` replies with whatever `Cookie` header the
    /// request carried (or `none`).
    fn spawn_cookie_server() -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).ok();
                    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut cookie = None;
                    loop {
                        let mut l = String::new();
                        reader.read_line(&mut l).ok();
                        if l.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.trim_end().split_once(':') {
                            if k.eq_ignore_ascii_case("cookie") {
                                cookie = Some(v.trim().to_string());
                            }
                        }
                    }
                    let (extra, body) = if path == "/set" {
                        ("Set-Cookie: sid=js; Path=/\r\n", "set".to_string())
                    } else {
                        ("", cookie.unwrap_or_else(|| "none".to_string()))
                    };
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len()).ok();
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn fetch_and_xhr_share_the_contexts_cookie_jar_and_a_different_context_is_isolated() {
        let base = spawn_cookie_server();
        let ctx = NetworkContext::new();

        // Environment A: a fetch() whose response sets a cookie.
        // Both pages are on the API's own origin (script requests carry
        // cookies same-origin by default).
        let page = format!("{base}/page.html");
        let mut a = JsEnvironment::new_with_network(make_doc(), Some(&page), ctx.clone());
        a.run_script(&format!("fetch('{base}/set')")).unwrap();
        poll_until_no_pending_fetches(&mut a);

        // Environment B — a different page, same profile context: its
        // fetch() and its XMLHttpRequest both send the cookie.
        let mut b = JsEnvironment::new_with_network(make_doc(), Some(&page), ctx.clone());
        b.run_script(&format!(
            "globalThis.viaFetch = null; globalThis.viaXhr = null; \
             fetch('{base}/echo').then(r => r.text()).then(t => {{ globalThis.viaFetch = t; }}); \
             var x = new XMLHttpRequest(); x.open('GET', '{base}/echo'); \
             x.onload = function() {{ globalThis.viaXhr = x.responseText; }}; x.send();"
        ))
        .unwrap();
        poll_until_no_pending_fetches(&mut b);
        assert_eq!(b.run_script("globalThis.viaFetch").unwrap(), "sid=js");
        assert_eq!(b.run_script("globalThis.viaXhr").unwrap(), "sid=js");

        // Environment C on a DIFFERENT context sees no cookie.
        let mut c = JsEnvironment::new_with_network(make_doc(), Some(&page), NetworkContext::new());
        c.run_script(&format!("globalThis.r = null; fetch('{base}/echo').then(r => r.text()).then(t => {{ globalThis.r = t; }});")).unwrap();
        poll_until_no_pending_fetches(&mut c);
        assert_eq!(c.run_script("globalThis.r").unwrap(), "none");
    }

    // ---- Milestone 27: a blocked cross-origin read is observable ----

    #[test]
    fn a_blocked_cross_origin_response_reports_the_sop_guard_and_an_allowed_one_does_not() {
        use diagnostics::{collector::capture, Guard};
        let blocked_port = spawn_mock_server_with_headers("secret", "");
        let (_, c) = capture(|| {
            let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
            env.run_script(&format!("fetch('http://127.0.0.1:{blocked_port}').catch(e => 0);")).unwrap();
            poll_until_no_pending_fetches(&mut env);
        });
        assert!(c.saw_guard(Guard::SopBlocked), "{:?}", c.events);

        let allowed_port = spawn_mock_server_with_headers("public", "Access-Control-Allow-Origin: *\r\n");
        let (_, c) = capture(|| {
            let mut env = JsEnvironment::new_with_page_url(make_doc(), Some("https://example.test/page.html"));
            env.run_script(&format!("fetch('http://127.0.0.1:{allowed_port}').catch(e => 0);")).unwrap();
            poll_until_no_pending_fetches(&mut env);
        });
        assert!(!c.saw_guard(Guard::SopBlocked), "a CORS-permitted response is not a guard trip");
    }

    #[test]
    fn the_pending_fetches_gauge_tracks_in_flight_requests() {
        let base = spawn_cookie_server();
        let mut env = JsEnvironment::new(make_doc());
        env.run_script(&format!("fetch('{base}/echo');")).unwrap();
        // Set at dispatch (>= 1 immediately) and back down once drained.
        assert!(env.has_pending_fetches());
        poll_until_no_pending_fetches(&mut env);
        assert!(!env.has_pending_fetches());
    }

    // ---- Hardening pass: script execution limits and storage quota ----

    #[test]
    fn an_infinite_loop_is_stopped_instead_of_hanging_the_ui_thread() {
        use diagnostics::{collector::capture, Guard};
        let mut env = JsEnvironment::new(make_doc());
        let started = std::time::Instant::now();
        let (result, c) = capture(|| env.run_script("while (true) {}"));
        let error = result.expect_err("an infinite loop must be stopped");
        assert!(error.contains("RuntimeLimit"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "stopped in bounded time: {:?}", started.elapsed());
        assert!(c.saw_guard(Guard::ScriptRuntimeLimit));
        // The environment is still usable afterwards.
        assert_eq!(env.run_script("1 + 1").unwrap(), "2");
    }

    #[test]
    fn a_script_cannot_swallow_the_runtime_limit_with_try_catch() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("try { while (true) {} } catch (e) { 'swallowed' }");
        assert!(result.is_err(), "the limit is not a catchable exception: {result:?}");
    }

    #[test]
    fn runaway_loops_in_callbacks_are_stopped_and_reported_too() {
        use diagnostics::{collector::capture, Guard};
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("requestAnimationFrame(function () { for (;;) {} });").unwrap();
        let (_, c) = capture(|| env.run_animation_frame_callbacks(0.0));
        assert!(c.saw_guard(Guard::ScriptRuntimeLimit), "an rAF callback's runaway loop is reported, not swallowed: {:?}", c.events);
        assert_eq!(env.run_script("2 + 2").unwrap(), "4");
    }

    #[test]
    fn infinite_recursion_is_a_bounded_error() {
        let mut env = JsEnvironment::new(make_doc());
        let result = env.run_script("function f() { return f() + 1 } f()");
        assert!(result.is_err_and(|e| e.contains("RuntimeLimit")));
    }

    #[test]
    fn ordinary_long_loops_are_unaffected() {
        let mut env = JsEnvironment::new(make_doc());
        assert_eq!(env.run_script("var n = 0; for (var i = 0; i < 1000000; i++) { n += i } n").unwrap(), "499999500000");
    }

    #[test]
    fn storage_refuses_writes_past_the_quota_and_leaves_the_store_unchanged() {
        use diagnostics::{collector::capture, Guard};
        for area in ["localStorage", "sessionStorage"] {
            let mut env = JsEnvironment::new(make_doc());
            let (result, c) = capture(|| {
                env.run_script(&format!(
                    "var big = 'x'.repeat(1 << 20); var stored = 0; var threw = 'no'; \
                     try {{ for (var i = 0; i < 64; i++) {{ {area}.setItem('k' + i, big); stored++; }} }} \
                     catch (e) {{ threw = String(e); }} \
                     [stored, threw.indexOf('QuotaExceededError') >= 0].join(',')"
                ))
            });
            // 5 MiB quota, 1 MiB values: exactly 4 fit (each also counts its key).
            assert_eq!(result.unwrap(), "4,true", "{area}");
            assert!(c.saw_guard(Guard::StorageQuotaExceeded));
            // Refused writes left the store as it was; smaller writes and
            // in-place replacement still work.
            assert_eq!(env.run_script(&format!("{area}.getItem('k4')")).unwrap(), "null");
            assert_eq!(env.run_script(&format!("{area}.setItem('k0', 'small'); {area}.getItem('k0')")).unwrap(), "small");
            assert_eq!(env.run_script(&format!("{area}.setItem('tiny', 'ok'); {area}.getItem('tiny')")).unwrap(), "ok");
        }
    }

    #[test]
    fn overwriting_a_key_does_not_double_count_its_old_value() {
        let mut env = JsEnvironment::new(make_doc());
        // 40 overwrites of a 1 MiB value must never exceed the quota.
        let result = env.run_script("var v = 'y'.repeat(1 << 20); for (var i = 0; i < 40; i++) { localStorage.setItem('same', v) } 'ok'");
        assert_eq!(result.unwrap(), "ok");
    }

    #[test]
    fn form_submission_with_a_catastrophic_pattern_completes_immediately() {
        // Hardening pass: `(a+)+$` on this value used to take seconds to
        // forever inside submit validation.
        let doc = make_doc_with_form_field(false, Some("(a+)+$"));
        {
            let mut root = doc.borrow_mut();
            { let node = root.node_id_of("field").unwrap(); root.set_attribute(node, "value", &format!("{}!", "a".repeat(5000))); }
        }
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.run_script("globalThis.submitted = false; document.getElementById('f').addEventListener('submit', () => { globalThis.submitted = true; });").unwrap();

        let started = std::time::Instant::now();
        env.dispatch_click(&full_hit_path_for(&doc, "go"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "took {:?}", started.elapsed());
        assert_eq!(env.run_script("globalThis.submitted").unwrap(), "false", "the value does not match, so submission is blocked");
    }

    // ---- Hardening pass: localStorage is partitioned by origin ----

    fn env_for(url: Option<&str>, store: &LocalStorageStore) -> JsEnvironment {
        let page = match url {
            Some(url) => network::Initiator::for_url(url),
            None => network::Initiator::document(network::Origin::new_opaque(), None),
        };
        JsEnvironment::new_with_store(make_doc(), page, NetworkContext::new(), store)
    }

    #[test]
    fn local_storage_is_shared_within_an_origin_and_isolated_between_origins() {
        let store = LocalStorageStore::new();
        let mut a1 = env_for(Some("https://a.example/one"), &store);
        a1.run_script("localStorage.setItem('user', 'alice')").unwrap();

        // Another page (a later navigation) of the SAME origin sees it.
        let mut a2 = env_for(Some("https://a.example/two?x=1#f"), &store);
        assert_eq!(a2.run_script("localStorage.getItem('user')").unwrap(), "alice");
        // ...and a write there is visible back.
        a2.run_script("localStorage.setItem('n', '2')").unwrap();
        assert_eq!(a1.run_script("localStorage.getItem('n')").unwrap(), "2");

        // A different origin — other host, other scheme, other port — sees nothing.
        for other in ["https://b.example/", "http://a.example/", "https://a.example:8443/", "https://sub.a.example/"] {
            let mut env = env_for(Some(other), &store);
            assert_eq!(env.run_script("localStorage.getItem('user')").unwrap(), "null", "{other} must not see a.example's storage");
        }
        assert_eq!(store.origin_count(), 5);
    }

    #[test]
    fn opaque_origins_never_share_local_storage() {
        let store = LocalStorageStore::new();
        let mut one = env_for(None, &store);
        one.run_script("localStorage.setItem('k', 'v')").unwrap();
        let mut two = env_for(None, &store);
        assert_eq!(two.run_script("localStorage.getItem('k')").unwrap(), "null");
        assert_eq!(store.origin_count(), 0, "opaque origins are not recorded");
    }

    #[test]
    fn session_storage_is_always_per_page_even_within_an_origin() {
        let store = LocalStorageStore::new();
        let mut one = env_for(Some("https://a.example/"), &store);
        one.run_script("sessionStorage.setItem('k', 'v')").unwrap();
        let mut two = env_for(Some("https://a.example/"), &store);
        assert_eq!(two.run_script("sessionStorage.getItem('k')").unwrap(), "null");
    }

    #[test]
    fn the_quota_applies_to_the_shared_origin_area_across_pages() {
        let store = LocalStorageStore::new();
        let mut one = env_for(Some("https://a.example/"), &store);
        one.run_script("var big = 'x'.repeat(1 << 20); for (var i = 0; i < 4; i++) { localStorage.setItem('k' + i, big) }").unwrap();
        // A second page of the same origin cannot exceed the SAME quota.
        let mut two = env_for(Some("https://a.example/other"), &store);
        let result = two.run_script("var big = 'x'.repeat(1 << 20); try { localStorage.setItem('more', big); 'stored' } catch (e) { 'refused' }");
        assert_eq!(result.unwrap(), "refused");
    }

    #[test]
    fn the_origin_map_is_bounded() {
        let store = LocalStorageStore::new();
        for i in 0..600 {
            let mut env = env_for(Some(&format!("https://site{i}.example/")), &store);
            env.run_script("localStorage.setItem('k', 'v')").unwrap();
        }
        assert!(store.origin_count() <= 512);
    }

    // ---- Hardening pass: script-visible time is coarsened ----

    #[test]
    fn animation_frame_timestamps_are_coarsened_to_whole_milliseconds() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("globalThis.seen = []; function f(t) { globalThis.seen.push(t); } requestAnimationFrame(f);").unwrap();
        env.run_animation_frame_callbacks(1234.5678901);
        assert_eq!(env.run_script("String(globalThis.seen[0])").unwrap(), "1234", "no sub-millisecond resolution leaks to script");

        let policy = privacy::FingerprintPolicy::standard();
        for (input, expected) in [(f64::NAN, 0.0), (f64::INFINITY, 0.0), (-5.0, 0.0), (0.999, 0.0), (16.6667, 16.0)] {
            assert_eq!(policy.coarsen_ms(input), expected, "{input}");
        }
    }

    // ---- Milestone 28: origin-based policy from script ----

    /// A test server whose handler sees the request path and `Cookie`
    /// header and returns `(status, extra header lines, body)`.
    fn spawn_routed_server<F>(handler: F) -> String
    where
        F: Fn(&str, Option<&str>) -> (u16, String, String) + Send + Sync + 'static,
    {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = std::sync::Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let handler = std::sync::Arc::clone(&handler);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).ok();
                    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut cookie = None;
                    loop {
                        let mut l = String::new();
                        reader.read_line(&mut l).ok();
                        if l.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.trim_end().split_once(':') {
                            if k.eq_ignore_ascii_case("cookie") {
                                cookie = Some(v.trim().to_string());
                            }
                        }
                    }
                    let (status, headers, body) = handler(&path, cookie.as_deref());
                    write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len()).ok();
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Runs `script`, drives every fetch/XHR to completion, and returns
    /// `globalThis.r`.
    fn run_and_read(env: &mut JsEnvironment, script: &str) -> String {
        env.run_script(&format!("globalThis.r = 'unset'; {script}")).unwrap();
        poll_until_no_pending_fetches(env);
        env.run_script("globalThis.r").unwrap()
    }

    const READ: &str = ".then(x => x.text()).then(t => { globalThis.r = t; }).catch(e => { globalThis.r = 'blocked'; })";

    #[test]
    fn a_page_with_no_url_has_an_opaque_origin_not_a_free_pass() {
        // Before M28, `page_url: None` meant the same-origin check was
        // skipped entirely: the built-in page could read any response.
        let base = spawn_routed_server(|path, _| match path {
            "/star" => (200, "Access-Control-Allow-Origin: *\r\n".into(), "public".into()),
            "/null" => (200, "Access-Control-Allow-Origin: null\r\n".into(), "for-null".into()),
            _ => (200, String::new(), "private".into()),
        });
        let mut env = JsEnvironment::new(make_doc());
        assert!(env.origin().is_opaque());
        assert_eq!(run_and_read(&mut env, &format!("fetch('{base}/private'){READ}")), "blocked");
        assert_eq!(run_and_read(&mut env, &format!("fetch('{base}/star'){READ}")), "public");
        assert_eq!(
            run_and_read(&mut env, &format!("fetch('{base}/null'){READ}")),
            "blocked",
            "ACAO: null never grants access — every opaque origin serialises to null"
        );
    }

    #[test]
    fn credentials_mode_and_cors_with_credentials_for_a_same_site_api() {
        // Page and API on the same SITE (127.0.0.1) but different ORIGINS
        // (ports), so SameSite does not interfere and only the credentials
        // mode and CORS decide.
        let page_origin = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_origin = std::sync::Arc::clone(&page_origin);
        let api = spawn_routed_server(move |path, cookie| {
            let origin = seen_origin.lock().unwrap().clone();
            let headers = match path {
                "/exact" => format!("Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Credentials: true\r\n"),
                "/star" => "Access-Control-Allow-Origin: *\r\n".to_string(),
                _ => String::new(),
            };
            (200, headers, cookie.unwrap_or("none").to_string())
        });
        let page_server = spawn_routed_server(|_, _| (200, String::new(), String::new()));
        *page_origin.lock().unwrap() = page_server.clone();

        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("sid=secret", "127.0.0.1");
        let mut env = JsEnvironment::new_with_network(make_doc(), Some(&format!("{page_server}/app")), ctx);

        // Default mode: no cookie goes cross-origin; `*` makes it readable.
        assert_eq!(run_and_read(&mut env, &format!("fetch('{api}/star'){READ}")), "none");
        // `include`: the cookie goes, but `*` is not enough to read a credentialed response...
        assert_eq!(run_and_read(&mut env, &format!("fetch('{api}/star', {{credentials: 'include'}}){READ}")), "blocked");
        // ...an exact origin plus Allow-Credentials is.
        assert_eq!(run_and_read(&mut env, &format!("fetch('{api}/exact', {{credentials: 'include'}}){READ}")), "sid=secret");
        // `omit`: nothing even to the page's own origin.
        assert_eq!(run_and_read(&mut env, &format!("fetch('{api}/exact', {{credentials: 'omit'}}){READ}")), "none");

        // XHR: `withCredentials` is the same switch.
        let xhr = |with: bool| {
            format!(
                "var x = new XMLHttpRequest(); x.open('GET', '{api}/exact'); x.withCredentials = {with}; \
                 x.onload = function() {{ globalThis.r = x.status === 0 ? 'blocked' : x.responseText; }}; x.send();"
            )
        };
        assert_eq!(run_and_read(&mut env, &xhr(false)), "none");
        assert_eq!(run_and_read(&mut env, &xhr(true)), "sid=secret");
    }

    #[test]
    fn a_same_origin_url_that_redirects_cross_origin_is_judged_by_where_it_landed() {
        let other = spawn_routed_server(|_, _| (200, String::new(), "other-origin secret".into()));
        let other_for_redirect = other.clone();
        let own = spawn_routed_server(move |path, _| match path {
            "/hop" => (302, format!("Location: {other_for_redirect}/data\r\n"), String::new()),
            _ => (200, String::new(), "mine".into()),
        });
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("{own}/page.html")));
        assert_eq!(run_and_read(&mut env, &format!("fetch('{own}/mine'){READ}")), "mine");
        assert_eq!(
            run_and_read(&mut env, &format!("fetch('{own}/hop'){READ}")),
            "blocked",
            "the response came from another origin; the requested URL's origin does not make it readable"
        );
    }

    #[test]
    fn relative_script_urls_resolve_against_the_page() {
        let base = spawn_routed_server(|path, _| (200, String::new(), format!("path={path}")));
        let mut env = JsEnvironment::new_with_page_url(make_doc(), Some(&format!("{base}/dir/page.html")));
        assert_eq!(run_and_read(&mut env, &format!("fetch('api?x=1'){READ}")), "path=/dir/api?x=1");
        assert_eq!(run_and_read(&mut env, &format!("fetch('/root'){READ}")), "path=/root");
        let xhr = "var x = new XMLHttpRequest(); x.open('GET', '/xhr'); x.onload = function() { globalThis.r = x.responseText; }; x.send();";
        assert_eq!(run_and_read(&mut env, xhr), "path=/xhr");
        // A page with no URL has no base: a relative fetch rejects.
        let mut opaque = JsEnvironment::new(make_doc());
        assert_eq!(run_and_read(&mut opaque, &format!("fetch('/root'){READ}")), "blocked");
    }

    // ---- Milestone 29: one definition of "text-editable" ----

    #[test]
    fn typing_with_a_checkbox_focused_does_not_edit_its_value() {
        // `js` used to treat every `<input>` as a text field, so keys typed
        // while a checkbox (or submit button) had focus were appended to its
        // `value` attribute. It now asks `dom::Element::is_text_editable`.
        let mut root = Element::new("div");
        let mut checkbox = Element::new("input");
        checkbox.set_attr("id", "agree");
        checkbox.set_attr("type", "checkbox");
        checkbox.set_attr("value", "yes");
        root.append_child(checkbox);
        let doc = Rc::new(RefCell::new(Document::new(root)));
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        env.set_focus(Some("agree".to_string()));
        for key in ["x", "Backspace", "y"] {
            env.dispatch_keydown(key);
        }
        assert_eq!(doc.borrow().find_by_id("agree").and_then(|e| e.attr("value")), Some("yes"));
    }

    // ---- Performance audit: the detached forest ----

    fn forest_visits<R>(env: &mut JsEnvironment, f: impl FnOnce(&mut JsEnvironment) -> R) -> (R, u64) {
        let before = env.host().detached_visits();
        let r = f(env);
        let after = env.host().detached_visits();
        (r, after - before)
    }

    /// Creating, configuring and nesting N detached elements is linear in
    /// N. (Every lookup used to scan the whole forest: O(N²).)
    #[test]
    fn building_a_detached_tree_of_n_elements_is_linear() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        const N: u64 = 3000;
        let (result, visits) = forest_visits(&mut env, |env| {
            env.run_script(&format!(
                "var list = document.createElement('ul'); \
                 for (var i = 0; i < {N}; i++) {{ \
                   var item = document.createElement('li'); \
                   item.setAttribute('data-i', '' + i); \
                   item.textContent = 'item ' + i; \
                   list.appendChild(item); \
                   item.setAttribute('class', 'row'); \
                   item.getAttribute('data-i'); \
                 }} \
                 list.setAttribute('id', 'the-list'); \
                 document.getElementById('headline').appendChild(list);"
            ))
        });
        result.unwrap();
        assert!(visits < 20 * N, "{visits} element visits for {N} elements");
        let doc = doc.borrow();
        let list = doc.find_by_id("the-list").expect("attached");
        assert_eq!(list.children().len() as u64, N);
        assert_eq!(env.host().detached_node_count(), 0, "everything was attached");
    }

    /// Elements a script created and then dropped are freed once the
    /// garbage collector drops their handles (they used to stay for the
    /// life of the page).
    #[test]
    fn detached_elements_no_script_can_reach_are_freed() {
        let mut env = JsEnvironment::new(make_doc());
        env.run_script("for (var i = 0; i < 20000; i++) { var e = document.createElement('div'); e.setAttribute('n', '' + i); } var keep = document.createElement('p');")
            .unwrap();
        boa_gc::force_collect();
        // The next forest operation processes what the collector dropped.
        env.run_script("var another = document.createElement('i'); keep.setAttribute('still', 'here');").unwrap();
        let held = env.host().detached_root_count();
        assert!(held <= 3, "{held} detached roots still held; only `e`, `keep` and `another` are reachable");
        assert_eq!(env.run_script("keep.getAttribute('still')").unwrap(), "here", "reachable elements are kept");
    }

    #[test]
    fn appending_an_ancestor_into_its_own_descendant_is_refused() {
        let doc = make_doc();
        let mut env = JsEnvironment::new(Rc::clone(&doc));
        let refused = env
            .run_script(
                "var a = document.createElement('div'); var b = document.createElement('div'); \
                 a.appendChild(b); var refused = 'no'; \
                 try { b.appendChild(a); } catch (e) { refused = e.message; } \
                 a.setAttribute('id', 'a'); \
                 document.getElementById('headline').appendChild(a); refused",
            )
            .unwrap();
        assert!(refused.starts_with("HierarchyRequestError"), "{refused}");
        let doc = doc.borrow();
        let a = doc.find_by_id("a").expect("a attached");
        assert_eq!(a.children().len(), 1, "a still holds b; no cycle was made");
    }

// ---- Milestone 36: script reaches only what its capabilities hold (ADR 010) ----

/// A route that counts the requests that reach it and answers each at once.
struct CountingRoute(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl network::NetworkRoute for CountingRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, network::NetworkError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(FetchResponse { status: 200, final_url: url.to_string(), headers: Vec::new(), body: b"hello".to_vec() })
    }
}

fn counting_network() -> (NetworkContext, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let route = std::sync::Arc::new(CountingRoute(std::sync::Arc::clone(&hits)));
    (NetworkContext::with_route(move |_store| route), hits)
}

fn page_at(url: &str) -> Initiator {
    Initiator::for_url(url)
}

/// Drains the page's fetches (the counting route answers at once, on the
/// fetch pool).
fn settle(env: &mut JsEnvironment) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while env.has_pending_fetches() && std::time::Instant::now() < deadline {
        env.poll_pending_fetches();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    env.poll_pending_fetches();
}

#[test]
fn a_page_without_network_cannot_fetch_and_gets_a_capability_error() {
    let before = diagnostics::metrics::snapshot();
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), ScriptCapabilities::default());
    env.run_script("globalThis.outcome = 'pending'; fetch('https://a.example/data').then(() => { outcome = 'fetched' }, e => { outcome = String(e) })")
        .unwrap();
    assert!(!env.has_pending_fetches(), "nothing was sent");
    env.poll_pending_fetches();
    let outcome = env.run_script("outcome").unwrap();
    assert!(outcome.contains("NotAllowedError") && outcome.contains("network capability"), "a capability error, not a network one: {outcome}");
    assert!(!outcome.contains("network error"), "{outcome}");
    assert!(diagnostics::metrics::snapshot().delta(&before, "guard.capability_denied") >= 1);
}

#[test]
fn xhr_send_without_network_throws_a_capability_error() {
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), ScriptCapabilities::default());
    let result = env
        .run_script("var x = new XMLHttpRequest(); x.open('GET', '/data'); try { x.send(); 'sent' } catch (e) { String(e) }")
        .unwrap();
    assert!(result.contains("NotAllowedError"), "{result}");
    assert!(!env.has_pending_fetches());
}

#[test]
fn with_the_capability_a_fetch_reaches_the_network() {
    let (network, hits) = counting_network();
    let authority = capability::Authority::new();
    let capabilities = ScriptCapabilities { network: Some(authority.network(network)), local_storage: None, ..ScriptCapabilities::default() };
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), capabilities);
    env.run_script("globalThis.body = ''; fetch('/data').then(r => r.text()).then(t => { body = t })").unwrap();
    settle(&mut env);
    assert_eq!(env.run_script("body").unwrap(), "hello");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn revoking_the_network_refuses_later_and_in_flight_requests() {
    let (network, hits) = counting_network();
    let authority = capability::Authority::new();
    let granted = authority.network(network);
    let capabilities = ScriptCapabilities { network: Some(granted.clone()), local_storage: None, ..ScriptCapabilities::default() };
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), capabilities);
    env.run_script("globalThis.first = 'pending'; fetch('/one').then(() => { first = 'resolved' }, e => { first = String(e) })").unwrap();
    // Revoked while the request is in flight: the response, whenever it
    // lands, never reaches script.
    granted.revoke();
    settle(&mut env);
    let first = env.run_script("first").unwrap();
    assert!(first.contains("revoked"), "{first}");
    env.run_script("globalThis.second = 'pending'; fetch('/two').then(() => { second = 'resolved' }, e => { second = String(e) })").unwrap();
    settle(&mut env);
    assert!(env.run_script("second").unwrap().contains("NotAllowedError"));
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) <= 1, "the second request never left");
}

#[test]
fn without_storage_local_storage_throws_but_session_storage_works() {
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), ScriptCapabilities::default());
    for call in ["localStorage.getItem('k')", "localStorage.setItem('k', 'v')", "localStorage.removeItem('k')", "localStorage.clear()"] {
        let result = env.run_script(&format!("try {{ {call}; 'allowed' }} catch (e) {{ String(e) }}")).unwrap();
        assert!(result.contains("SecurityError"), "{call}: {result}");
    }
    env.run_script("sessionStorage.setItem('k', 'v')").unwrap();
    assert_eq!(env.run_script("sessionStorage.getItem('k')").unwrap(), "v");
}

#[test]
fn a_storage_handle_for_another_origin_is_refused() {
    let authority = capability::Authority::new();
    let store = storage::StorageStore::new();
    let other = origin::StorageKey::first_party(&Origin::parse("https://victim.example/"));
    store.area_for(&other).set("secret".into(), "s3cret".into()).unwrap();
    let capabilities = ScriptCapabilities { network: None, local_storage: Some(authority.storage(store).scope(&other)), ..ScriptCapabilities::default() };
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page_at("https://attacker.example/"), capabilities);
    let result = env.run_script("try { localStorage.getItem('secret') } catch (e) { String(e) }").unwrap();
    assert!(result.contains("SecurityError"), "{result}");
    assert!(!result.contains("s3cret"));
}

#[test]
fn revoking_storage_takes_effect_on_the_next_call() {
    let authority = capability::Authority::new();
    let grant = authority.storage(storage::StorageStore::new());
    let page = page_at("https://a.example/");
    let key = origin::StorageKey::first_party(&page.origin);
    let capabilities = ScriptCapabilities { network: None, local_storage: Some(grant.scope(&key)), ..ScriptCapabilities::default() };
    let mut env = JsEnvironment::new_with_capabilities(make_doc(), page, capabilities);
    env.run_script("localStorage.setItem('k', 'v')").unwrap();
    assert_eq!(env.run_script("localStorage.getItem('k')").unwrap(), "v");
    grant.revoke();
    let result = env.run_script("try { localStorage.getItem('k') } catch (e) { String(e) }").unwrap();
    assert!(result.contains("SecurityError") && result.contains("revoked"), "{result}");
}

// ---- Milestone 37: what script observes goes through the fingerprint policy (ADR 019 §3) ----

fn env_with_policy(policy: privacy::FingerprintPolicy) -> JsEnvironment {
    let capabilities = ScriptCapabilities { fingerprint: policy, ..ScriptCapabilities::default() };
    JsEnvironment::new_with_capabilities(make_doc(), page_at("https://a.example/"), capabilities)
}

#[test]
fn scripts_observe_date_at_the_policys_resolution_and_timezone() {
    let mut env = env_with_policy(privacy::FingerprintPolicy::private());
    // Sampled many times: every reading is a multiple of 100 ms.
    let script = "var ok = true; for (var i = 0; i < 200; i++) { if (Date.now() % 100 !== 0 || new Date().getTime() % 100 !== 0) ok = false; } ok";
    assert_eq!(env.run_script(script).unwrap(), "true");
    // UTC: no offset, and local getters equal the UTC ones.
    assert_eq!(env.run_script("new Date(0).getTimezoneOffset()").unwrap(), "0");
    assert_eq!(env.run_script("new Date(Date.UTC(2026, 6, 1, 15, 30)).getHours()").unwrap(), "15");
    assert_eq!(env.run_script("new Date(2026, 0, 1).getTime() === Date.UTC(2026, 0, 1)").unwrap(), "true", "local time is UTC");
}

#[test]
fn the_standard_policy_keeps_the_real_timezone_and_millisecond_time() {
    use chrono::TimeZone;
    let mut env = env_with_policy(privacy::FingerprintPolicy::standard());
    // The machine's own offset for that instant (minutes west of UTC, as JS says it).
    let when = chrono::DateTime::from_timestamp_millis(1_782_000_000_000).unwrap().naive_utc();
    let offset_east = chrono::Local.offset_from_utc_datetime(&when).local_minus_utc() / 60;
    let observed = env.run_script("new Date(1782000000000).getTimezoneOffset()").unwrap();
    assert_eq!(observed, (-offset_east).to_string());
    let script = "var any = false; for (var i = 0; i < 2000 && !any; i++) { if (Date.now() % 100 !== 0) any = true; } any";
    assert_eq!(env.run_script(script).unwrap(), "true", "1 ms resolution, not 100");
}

#[test]
fn animation_frame_timestamps_follow_the_policy() {
    let mut env = env_with_policy(privacy::FingerprintPolicy::private());
    env.run_script("globalThis.seen = []; requestAnimationFrame(function (t) { seen.push(t); });").unwrap();
    env.run_animation_frame_callbacks(1234.5678);
    assert_eq!(env.run_script("String(seen[0])").unwrap(), "1200");
}

#[test]
fn no_script_api_exposes_the_screen_or_the_machine() {
    // ADR 019 §3: if one of these is ever added it must report bucketed
    // values through the policy; until then, they do not exist.
    let mut env = JsEnvironment::new(make_doc());
    for name in ["screen", "innerWidth", "innerHeight", "outerWidth", "devicePixelRatio", "navigator", "performance", "matchMedia"] {
        assert_eq!(env.run_script(&format!("typeof globalThis.{name}")).unwrap(), "undefined", "{name}");
    }
}

// ---- Milestone 39: the full mutation API from script (ADR 021 §7) ----

/// `<ul id=list><li id=a>A</li><li id=b>B</li><li id=c>C</li></ul>`.
fn make_list() -> Rc<RefCell<Document>> {
    let mut list = Element::new("ul");
    list.set_attr("id", "list");
    for id in ["a", "b", "c"] {
        let mut li = Element::new("li");
        li.set_attr("id", id);
        li.children.push(Node::Text(id.to_uppercase()));
        list.children.push(Node::Element(li));
    }
    let mut root = Element::new("div");
    root.children.push(Node::Element(list));
    Rc::new(RefCell::new(Document::new(root)))
}

fn list_text(doc: &Rc<RefCell<Document>>) -> String {
    doc.borrow().find_by_id("list").unwrap().text_content()
}

#[test]
fn insert_before_places_a_created_element_and_a_text_node() {
    let doc = make_list();
    let mut env = JsEnvironment::new(Rc::clone(&doc));
    env.run_script(
        "var list = document.getElementById('list'); \
         var x = document.createElement('li'); x.textContent = 'X'; \
         list.insertBefore(x, document.getElementById('b')); \
         list.insertBefore(document.createTextNode('!'), null);",
    )
    .unwrap();
    assert_eq!(list_text(&doc), "AXBC!");
    assert!(env.take_dirty());
    assert!(doc.borrow().index_is_consistent());
}

#[test]
fn remove_child_returns_a_handle_that_can_be_inserted_again() {
    let doc = make_list();
    let mut env = JsEnvironment::new(Rc::clone(&doc));
    env.run_script(
        "var list = document.getElementById('list'); \
         var removed = list.removeChild(document.getElementById('a')); \
         list.appendChild(removed);",
    )
    .unwrap();
    assert_eq!(list_text(&doc), "BCA", "the same node, moved to the end through removal");
    assert!(doc.borrow().find_by_id("a").is_some());
    assert!(doc.borrow().index_is_consistent());
}

#[test]
fn append_child_of_an_attached_node_moves_it() {
    let doc = make_list();
    let mut env = JsEnvironment::new(Rc::clone(&doc));
    let a_before = doc.borrow().node_id_of("a");
    env.run_script("var list = document.getElementById('list'); list.appendChild(document.getElementById('a'));").unwrap();
    assert_eq!(list_text(&doc), "BCA");
    assert_eq!(doc.borrow().node_id_of("a"), a_before, "moved, not copied");
    // An ancestor never moves under its own descendant (a
    // `HierarchyRequestError`, as in browsers).
    let err = env.run_script("document.getElementById('a').appendChild(document.getElementById('list'));").unwrap_err();
    assert!(err.contains("HierarchyRequestError"), "{err}");
    assert_eq!(list_text(&doc), "BCA");
    assert!(doc.borrow().index_is_consistent());
}

#[test]
fn replace_child_and_clone_node() {
    let doc = make_list();
    let mut env = JsEnvironment::new(Rc::clone(&doc));
    env.run_script(
        "var list = document.getElementById('list'); \
         var copy = document.getElementById('c').cloneNode(true); \
         copy.setAttribute('id', 'c2'); \
         var old = list.replaceChild(copy, document.getElementById('b')); \
         list.appendChild(old);",
    )
    .unwrap();
    assert_eq!(list_text(&doc), "ACCB");
    let doc = doc.borrow();
    assert_ne!(doc.node_id_of("c2"), doc.node_id_of("c"), "a clone is a new node");
    assert!(doc.index_is_consistent());
}

#[test]
fn hostile_arguments_change_nothing() {
    // Each call is refused with an exception and changes nothing. A node is
    // known by its wrapper's internal slot, so an object claiming a node id
    // (`{__node_id: …}`, how M18–M39 recognised node arguments) is no node.
    let doc = make_list();
    let mut env = JsEnvironment::new(Rc::clone(&doc));
    let errors = env
        .run_script(
            "var list = document.getElementById('list'); var errors = []; \
             var calls = [ \
               () => list.insertBefore({}, null), () => list.insertBefore(null, null), \
               () => list.removeChild({__node_id: 1}), () => list.removeChild(document.getElementById('list')), \
               () => list.replaceChild(document.createElement('i'), {__node_id: 1e300}), \
               () => list.insertBefore(document.createElement('i'), document.getElementById('list')), \
               () => list.appendChild(), () => Object.getPrototypeOf(list).appendChild.call({}, list) ]; \
             for (var c of calls) { try { c(); errors.push('none'); } catch (e) { errors.push(e.name); } } \
             errors.join(',')",
        )
        .unwrap();
    assert_eq!(errors, "TypeError,TypeError,TypeError,Error,TypeError,Error,TypeError,TypeError");
    assert_eq!(list_text(&doc), "ABC");
    assert!(doc.borrow().index_is_consistent());
}

// ---- Milestone 40: the binding cases every runtime passes (ADR 023 §7) ----

#[test]
fn boa_passes_every_binding_case() {
    let make = |document: Rc<RefCell<Document>>| JsEnvironment::new(document).into_inner();
    let failures: Vec<String> = script::cases::CASES.iter().filter_map(|case| script::cases::run(case, &make).err()).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `getElementById` returns the same wrapper for the same node (M40's
/// acceptance criterion) and makes it once: it used to build a new wrapper
/// of about twenty function objects on every call.
#[test]
fn get_element_by_id_makes_one_wrapper_per_node_however_often_it_is_called() {
    let mut env = JsEnvironment::new(make_doc());
    let before = env.host().wrappers_opened();
    let same = env
        .run_script("var first = document.getElementById('headline'); var same = true; for (var i = 0; i < 10000; i++) { same = same && document.getElementById('headline') === first; } same")
        .unwrap();
    assert_eq!(same, "true");
    // The headline's wrapper (and the document's, made on first use).
    assert!(env.host().wrappers_opened() - before <= 2, "{} wrappers made", env.host().wrappers_opened() - before);
}

/// The runtime's wrapper cache holds dead entries only until it has
/// doubled: 20,000 created and dropped elements leave it bounded.
#[test]
fn the_wrapper_cache_does_not_grow_with_dead_wrappers() {
    let mut env = JsEnvironment::new(make_doc());
    for _ in 0..4 {
        env.run_script("for (var i = 0; i < 5000; i++) { document.createElement('div'); }").unwrap();
        env.collect_garbage();
    }
    env.run_script("document.createElement('p')").unwrap();
    assert!(env.host().detached_root_count() <= 2, "{} detached roots held", env.host().detached_root_count());
    assert_eq!(env.host().object_count(), 0);
}

/// M40's gap: a node in the document keeps its wrapper, so expando
/// properties set on it survive a collection — whether it was in the
/// document when first wrapped or was inserted later, and for the live
/// wrappers of an inserted subtree's descendants too.
#[test]
fn expandos_on_nodes_in_the_document_survive_garbage_collection() {
    let mut env = JsEnvironment::new(make_doc());
    env.run_script(
        "document.getElementById('headline').mark = 'kept'; \
         (function () { \
            var outer = document.createElement('div'); outer.id = 'outer'; outer.mark = 'outer'; \
            var inner = document.createElement('span'); inner.id = 'inner'; inner.mark = 'inner'; \
            outer.appendChild(inner); \
            document.getElementById('headline').appendChild(outer); \
         })();",
    )
    .unwrap();
    env.run_script("for (var i = 0; i < 5000; i++) { document.createElement('div'); }").unwrap();
    env.collect_garbage();
    let marks = env
        .run_script("[document.getElementById('headline').mark, document.getElementById('outer').mark, document.getElementById('inner').mark].join(',')")
        .unwrap();
    assert_eq!(marks, "kept,outer,inner");
}

/// A kept wrapper is let go once its node leaves the document: 200 wrapped,
/// marked and removed elements are freed after a collection.
#[test]
fn a_removed_nodes_wrapper_is_let_go() {
    let mut env = JsEnvironment::new(make_doc());
    env.run_script(
        "var h = document.getElementById('headline'); \
         var added = []; \
         for (var i = 0; i < 200; i++) { var d = document.createElement('div'); d.mark = i; h.appendChild(d); added.push(d); } \
         for (var i = 0; i < added.length; i++) { h.removeChild(added[i]); } \
         added = null; d = null;",
    )
    .unwrap();
    assert_eq!(env.host().detached_root_count(), 200, "removed, still referenced");
    env.run_script("document.createElement('p')").unwrap();
    env.collect_garbage();
    env.run_script("document.createElement('p')").unwrap();
    assert!(env.host().detached_root_count() <= 2, "{} removed elements still held", env.host().detached_root_count());
}

/// M40's gap: `window` and `self` are the global object, which is an
/// `EventTarget`.
#[test]
fn window_and_self_are_the_global_object() {
    let mut env = JsEnvironment::new(make_doc());
    let checks = env
        .run_script(
            "[window === globalThis, self === window, window.window === window, window.document === document, \
              typeof window.fetch, typeof window.addEventListener, window.localStorage === localStorage].join(',')",
        )
        .unwrap();
    assert_eq!(checks, "true,true,true,true,function,function,true");
    env.run_script("window.addEventListener('load', function () {}); var x = 1; window.x = 2;").expect("a listener on the window registers");
    assert_eq!(env.run_script("x").unwrap(), "2", "a global variable is a window property");
    assert!(env.run_script("EventTarget.prototype.addEventListener.call({}, 'x', null)").is_err(), "a plain object is not an EventTarget");
}
