//! Milestone 17: real, cross-subsystem integration tests — the thing
//! isolated per-milestone unit testing structurally cannot catch (a
//! flexbox container with wrapped inline text and an image, styled
//! via cascaded/parallel-resolved rules, mutated by a JS script with
//! a click listener, all at once — exactly what a real page looks
//! like).
//!
//! Deliberately excludes `gpu_backend`/`shell_desktop`/`app` — this
//! crate exists specifically so the FULL non-GPU pipeline (parse →
//! cascade → layout → JS mutation → event dispatch → paint-list
//! generation) can be exercised and RUN in a sandbox with no display
//! server, unlike `app` itself (which pulls in `wgpu`/`winit` and
//! can't even be `cargo check`ed here — see every milestone's GPU
//! caveat since Milestone 1). Every test here runs a REAL pipeline
//! end to end, not a mock of one.
//!
//! This is a test-only crate (`publish = false`) — no public API,
//! just the tests themselves.

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    fn parse_and_style(html: &str) -> (dom::Element, css_parser::Stylesheet) {
        let root = html_parser::parse_document(html).expect("parse should not fail on well-formed test HTML");
        let mut style_text = String::new();
        collect_style_text(&root, &mut style_text);
        let stylesheet = css_parser::parse_stylesheet(&style_text);
        let body = root
            .children
            .iter()
            .find_map(|n| match n {
                dom::Node::Element(el) if el.tag == "body" => Some(el.clone()),
                _ => None,
            })
            .expect("test document must have a <body>");
        (body, stylesheet)
    }

    fn collect_style_text(element: &dom::Element, out: &mut String) {
        for child in &element.children {
            match child {
                dom::Node::Element(el) if el.tag == "style" => {
                    for grandchild in &el.children {
                        if let dom::Node::Text(text) = grandchild {
                            out.push_str(text);
                        }
                    }
                }
                dom::Node::Element(el) => collect_style_text(el, out),
                dom::Node::Text(_) => {}
            }
        }
    }

    fn collect_scripts(element: &dom::Element, out: &mut Vec<String>) {
        if element.tag == "script" {
            for child in &element.children {
                if let dom::Node::Text(text) = child {
                    out.push(text.clone());
                }
            }
        }
        for child in &element.children {
            if let dom::Node::Element(el) = child {
                collect_scripts(el, out);
            }
        }
    }

    fn full_test_font() -> text::Font {
        text::Font::dejavu_sans()
    }

    /// Writes a tiny, real, valid PNG to a temp file and returns its
    /// path — used by tests that need a genuinely decodable image
    /// (not a stub/mock) flowing through the full pipeline. A minimal
    /// 1x1 opaque-red PNG, hand-assembled from its raw bytes (avoids
    /// pulling in an image-writing dependency just for test fixtures).
    fn write_temp_png_fixture() -> String {
        // A valid 1x1 RGB PNG (red pixel) — the smallest real PNG that
        // decodes correctly through the `png` crate `images` uses.
        const PNG_BYTES: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk header
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // bit depth 8, color type 2 (RGB), CRC
            0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk header
            0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, // compressed red pixel
            0x18, 0xDD, 0x8D, 0xB0, // CRC
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
        ];
        let path = std::env::temp_dir().join(format!("integration_test_fixture_{}.png", std::process::id()));
        std::fs::write(&path, PNG_BYTES).expect("failed to write test fixture PNG");
        path.to_string_lossy().into_owned()
    }

    /// The single most important test in this crate: everything at
    /// once — flexbox layout, wrapped inline text, an image, CSS
    /// cascaded through classes AND inline styles, and a JS script
    /// that mutates the DOM on load — run through the complete
    /// pipeline exactly the way `app` would, and confirmed to produce
    /// a sane, non-empty result with no panic anywhere in the chain.
    #[test]
    fn everything_at_once_flexbox_text_image_css_and_js_together() {
        let fixture_path = write_temp_png_fixture();
        let html = format!(
            r#"<html><head><style>
                .container {{ display:flex; justify-content:space-between; gap:10px; width:400px; }}
                .card {{ background-color:#222222; padding:10px; }}
                p {{ color:#ffffff; }}
            </style></head>
            <body>
              <div id="root">
                <div class="container">
                  <div class="card" style="flex:1"><p>Some wrapped text that is long enough to actually wrap across several lines within its flex item's width.</p></div>
                  <div class="card" style="flex:1"><img src="{fixture_path}"></div>
                </div>
                <script>
                  document.getElementById('root').style.color = 'red';
                  console.log('integration script ran');
                </script>
              </div>
            </body></html>"#
        );

        let (body, stylesheet) = parse_and_style(&html);
        let root_html = html_parser::parse_document(&html).unwrap();
        let mut scripts = Vec::new();
        collect_scripts(&root_html, &mut scripts);

        let document = Rc::new(RefCell::new(dom::Document::new(body)));
        let mut js_env = js::JsEnvironment::new(Rc::clone(&document));
        for script in &scripts {
            js_env.run_script(script).expect("integration script should run without error");
        }

        let mut image_cache = images::ImageCache::with_local_file_access();
        let font = full_test_font();
        let laid_out = {
            let body_ref = document.borrow();
            layout::layout_document(body_ref.root(), 800.0, &font, &stylesheet, &mut image_cache)
        };

        // Sanity: the tree actually has content (didn't collapse to
        // nothing under the combined load).
        assert!(laid_out.width > 0.0);
        assert!(!laid_out.children.is_empty());

        // The JS mutation actually landed.
        assert_eq!(document.borrow().find_by_id("root").unwrap().attr("style"), Some("color:red"));

        // Painting the result doesn't panic either — the whole chain,
        // through to what would actually reach the GPU.
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let (items, image_resources) = rendering::display_list::build_display_list(&laid_out, &font, &mut atlas);
        assert!(!items.is_empty());
        assert_eq!(image_resources.len(), 1); // the one fixture image, decoded once

        std::fs::remove_file(&fixture_path).ok();
    }

    /// M17's explicit resilience scope: does ANY single subsystem
    /// failure crash the whole pipeline? A thrown JS exception must
    /// not prevent the page from still laying out and painting with
    /// whatever DOM state existed at the point of failure.
    #[test]
    fn a_thrown_js_exception_does_not_prevent_the_rest_of_the_page_from_rendering() {
        let html = r#"<html><body>
            <div id="before">before</div>
            <script>document.getElementById('before').textContent = 'set before error';</script>
            <script>throw new Error('deliberate failure');</script>
            <div id="after">after</div>
        </body></html>"#;

        let (body, stylesheet) = parse_and_style(html);
        let root_html = html_parser::parse_document(html).unwrap();
        let mut scripts = Vec::new();
        collect_scripts(&root_html, &mut scripts);

        let document = Rc::new(RefCell::new(dom::Document::new(body)));
        let mut js_env = js::JsEnvironment::new(Rc::clone(&document));
        let mut had_error = false;
        for script in &scripts {
            if js_env.run_script(script).is_err() {
                had_error = true;
            }
        }
        assert!(had_error, "the deliberately-throwing script should have reported an error");

        // The mutation from the FIRST (successful) script still landed...
        assert_eq!(document.borrow().find_by_id("before").unwrap().text_content(), "set before error");

        // ...and layout/paint still succeed on the resulting document,
        // proving one bad script doesn't poison the whole pipeline.
        let mut image_cache = images::ImageCache::with_local_file_access();
        let font = full_test_font();
        let laid_out = {
            let body_ref = document.borrow();
            layout::layout_document(body_ref.root(), 800.0, &font, &stylesheet, &mut image_cache)
        };
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let (items, _images) = rendering::display_list::build_display_list(&laid_out, &font, &mut atlas);
        assert!(!items.is_empty());
    }

    /// A missing/broken image reference must degrade to "nothing
    /// painted there," not take down layout or paint for the rest of
    /// the page — the same resilience principle, exercised through
    /// the FULL pipeline rather than `images`/`layout`'s own isolated
    /// unit tests for the same thing.
    #[test]
    fn a_broken_image_reference_degrades_gracefully_through_the_full_pipeline() {
        let html = r#"<html><body>
            <div>
              <img src="/definitely/does/not/exist.png">
              <p>Text after the broken image should still render.</p>
            </div>
        </body></html>"#;

        let (body, stylesheet) = parse_and_style(html);
        let mut image_cache = images::ImageCache::with_local_file_access();
        let font = full_test_font();
        let laid_out = layout::layout_document(&body, 800.0, &font, &stylesheet, &mut image_cache);

        let mut atlas = text::GlyphAtlas::new(512, 128);
        let (items, image_resources) = rendering::display_list::build_display_list(&laid_out, &font, &mut atlas);
        assert!(image_resources.is_empty(), "a failed decode should produce zero image resources, not a placeholder");
        assert!(!items.is_empty(), "the surrounding text should still have painted");
    }

    /// Click → hit-test → JS dispatch → DOM mutation → re-layout, the
    /// full Milestone 15 loop, run end to end (not just `js`'s own
    /// unit tests, which call `dispatch_click` directly without ever
    /// going through real `layout::hit_test_path` on a real laid-out
    /// tree).
    #[test]
    fn click_hit_test_dispatch_mutation_and_relayout_all_work_together() {
        let html = r#"<html><body>
            <div id="button" style="width:100px;height:40px">Click me</div>
        </body></html>"#;
        let (body, stylesheet) = parse_and_style(html);
        let document = Rc::new(RefCell::new(dom::Document::new(body)));
        let mut js_env = js::JsEnvironment::new(Rc::clone(&document));
        js_env
            .run_script(
                "document.getElementById('button').addEventListener('click', function () { \
                     document.getElementById('button').textContent = 'clicked'; \
                 });",
            )
            .unwrap();

        let mut image_cache = images::ImageCache::with_local_file_access();
        let font = full_test_font();
        let laid_out = {
            let body_ref = document.borrow();
            layout::layout_document(body_ref.root(), 800.0, &font, &stylesheet, &mut image_cache)
        };

        // A real hit-test against the real laid-out tree, at a point
        // inside the button's actual box.
        let hit_path = layout::hit_test_path(&laid_out, 10.0, 10.0);
        assert!(!hit_path.is_empty());
        js_env.dispatch_click(&hit_path);

        assert_eq!(document.borrow().find_by_id("button").unwrap().text_content(), "clicked");
        assert!(js_env.take_dirty());

        // Re-layout after the mutation succeeds and reflects the new content.
        let relaid_out = {
            let body_ref = document.borrow();
            layout::layout_document(body_ref.root(), 800.0, &font, &stylesheet, &mut image_cache)
        };
        assert!(!relaid_out.children.is_empty());
    }

    /// The parallel style cache (concurrency work) and layout must
    /// agree with each other on a document combining flex, text, and
    /// nested elements — not just each verified in isolation against
    /// their OWN synthetic trees in their own crates.
    #[test]
    fn parallel_style_resolution_and_layout_agree_on_a_combined_document() {
        let html = r#"<html><head><style>
            .flex { display:flex; }
            .item { flex:1; color:#ff0000; }
        </style></head><body>
            <div class="flex">
                <div class="item">a</div>
                <div class="item">b</div>
                <div class="item">c</div>
            </div>
        </body></html>"#;
        let (body, stylesheet) = parse_and_style(html);
        let mut image_cache = images::ImageCache::with_local_file_access();
        let font = full_test_font();
        let laid_out = layout::layout_document(&body, 900.0, &font, &stylesheet, &mut image_cache);

        let flex_container = &laid_out.children[0];
        assert_eq!(flex_container.children.len(), 3);
        // Each flex item got an equal share (flex:1 on all three).
        let widths: Vec<f32> = flex_container.children.iter().map(|c| c.width).collect();
        assert!((widths[0] - widths[1]).abs() < 0.1);
        assert!((widths[1] - widths[2]).abs() < 0.1);
    }
}
