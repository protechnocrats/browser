//! One loaded page: its DOM, stylesheet, images, JavaScript environment,
//! animation state, last layout and scroll position (`Page`), and how a
//! page is built from a parsed document or replaced by a navigation.
//! Internal to the engine — hosts see only `PageId`s (ADR 005).
//!
//! The state is shared through `Rc` handles: everything here lives on one
//! thread (the page's own, from Milestone 30) and never crosses the API.

use crate::loader::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Longest the page thread waits for web fonts before a first frame
/// (`ImageLoading::BeforeFirstFrame`): well under `UNRESPONSIVE_AFTER`, since
/// it is spent inside a tick (ADR 013).
const IN_TICK_FONT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Everything that belongs to ONE loaded page/tab, grouped as a single
/// `Clone`-able handle instead of the nine-plus separate
/// `Rc<RefCell<..>>`/`Rc<Cell<..>>` locals `main()` used to juggle
/// (each of `build_scene`/`tick`/`handle_input` had to individually
/// `Rc::clone` every one it touched — one line per field, times three
/// closures). Cloning a `Page` is cheap (every field is already an
/// `Rc`-family handle, so this is just a bundle of refcount bumps),
/// and every clone still points at the SAME underlying state, so the
/// "every closure holding the Rc sees a navigation swap for free"
/// property this project has relied on since Milestone 21 is
/// unchanged — this is a reorganization of WHERE that state lives, not
/// a change to HOW it's shared.
///
/// This is also the deliberate seam a future multi-tab feature would
/// need: today there's exactly one `Page`; a tab list is `Vec<Page>`
/// away rather than a redesign of `main.rs`'s closures. Not building
/// tabs now — see `ROADMAP.md` §3 (M36) — just not making that later
/// work harder than it has to be.
#[derive(Clone)]
pub struct Page {
    /// Milestone 13: the SAME `Rc<RefCell<..>>` is shared between the
    /// JS environment (which mutates it) and the render loop (which
    /// reads it for layout) — that sharing is the entire architecture
    /// Milestone 13 rests on, not an implementation detail.
    pub document: Rc<RefCell<dom::Document>>,
    pub stylesheet: Rc<RefCell<css_parser::Stylesheet>>,
    pub image_cache: Rc<RefCell<images::ImageCache>>,
    /// Milestone 21: the page's own URL, if it has one (`--url` mode,
    /// or after a successful navigation) — `None` for the hardcoded
    /// demo, which has no real address a relative link/form action
    /// could resolve against.
    pub current_url: Rc<RefCell<Option<String>>>,
    pub js_env: Rc<RefCell<script::ScriptEnvironment>>,
    /// Milestone 22: persistent, cross-frame CSS `transition`/
    /// `@keyframes` state, threaded into `build_scene` each frame
    /// between cascade resolution and layout.
    pub transition_registry: Rc<RefCell<style::TransitionRegistry>>,
    pub animation_registry: Rc<RefCell<style::AnimationRegistry>>,
    /// Milestone 15: the most recently laid-out tree — `handle_input`'s
    /// hit-testing (its own callback, not inside `build_scene`) needs
    /// a `LayoutBox` tree to test against, and only `build_scene`
    /// produces one.
    pub last_layout: Rc<RefCell<Option<layout::LayoutBox>>>,
    /// Milestone 31: the page's styles, kept across frames and brought up to
    /// date incrementally from the document's mutation records.
    pub styles: Rc<RefCell<style::StyleEngine>>,
    /// Identifies the current stylesheet; bumped when a navigation replaces
    /// it, which makes the next restyle a full one.
    pub sheet_generation: Rc<Cell<u64>>,
    /// What the last frame was built from and drew — what decides whether
    /// the next one needs layout, and what its damage is measured against.
    pub last_frame: Rc<RefCell<Option<crate::frame::FrameMemo>>>,
    /// Milestone 31: what lets the next layout reuse unchanged subtrees of
    /// this one.
    pub layout_reuse: Rc<RefCell<layout::LayoutReuse>>,
    /// Milestone 15: whole-document (viewport) scroll offset.
    pub scroll_offset_y: Rc<Cell<f32>>,
    /// Milestone 33: the scroll offset of each scroll container
    /// (`overflow: auto|scroll`) the user has scrolled. Paint-time state:
    /// scrolling one never changes the layout.
    pub container_scrolls: Rc<RefCell<layout::ScrollOffsets>>,
    /// Milestone 33: where the pointer last was (viewport coordinates) —
    /// what a wheel scroll applies to.
    pub pointer: Rc<Cell<Option<(f32, f32)>>>,
    /// M4's gap: the element under the pointer and its ancestors (kept only
    /// when the stylesheet uses `:hover`), and the element being pressed and
    /// its ancestors — for `:hover` and `:active`.
    pub hover_path: Rc<RefCell<Vec<dom::NodeId>>>,
    pub active_path: Rc<RefCell<Vec<dom::NodeId>>>,
    /// Milestone 21: an in-flight page navigation, if one was
    /// triggered (`<a href>` click or a GET `<form>` submit) and
    /// hasn't resolved yet — polled once per tick. The `String`
    /// alongside the handle is the URL that was actually requested —
    /// kept so a successful navigation can update `current_url`
    /// (needed to resolve the NEXT relative link/action) without
    /// `fetch_page`/`NavigationResult` having to report the
    /// post-redirect URL back out. A real, narrow simplification: if
    /// the target redirects cross-path, `current_url` reflects the
    /// pre-redirect URL, not the post-redirect one.
    pub pending_nav: Rc<RefCell<Option<(String, NavigationHandle)>>>,
    /// Milestone 26: the profile's shared network context — one cookie
    /// jar and HTTP cache for this page's load, its JS `fetch()`/XHR,
    /// and every navigation it triggers. (A plain `Clone` handle, not
    /// an `Rc`: it is `Send + Sync` and crosses to background threads.)
    pub network: network::NetworkContext,
    /// Milestone 36: what this page's documents' scripts may reach (ADR
    /// 010). Each document gets the network capability as granted and the
    /// storage grant narrowed to its own storage key; revoking a kind here
    /// revokes it for the document showing now and every later one.
    pub grants: capability::PageGrants,
    /// Milestone 37: what this page's scripts may observe (ADR 019 §3).
    pub fingerprint: privacy::FingerprintPolicy,
    /// Whether navigations show the page before its images arrive.
    pub image_loading: Rc<Cell<ImageLoading>>,
    /// Images this document is still waiting for (progressive loading).
    pub pending_images: Rc<RefCell<Vec<PendingImage>>>,
    /// Elements showing an image that arrived since the last frame: laid
    /// out again (an `<img>`'s size comes from its image) and repainted.
    pub image_dirty: Rc<RefCell<Vec<dom::NodeId>>>,
    /// Milestone 35: this document's web fonts still loading, and those
    /// ready to join the page's font set (the page thread adds them).
    pub web_fonts: Rc<RefCell<Vec<crate::webfonts::WebFontLoad>>>,
    pub ready_fonts: Rc<RefCell<Vec<crate::webfonts::ReadyFont>>>,
    /// Bumped whenever a new document is installed: the page thread starts
    /// a fresh font set for it (a document's web fonts are its own).
    pub document_generation: Rc<Cell<u64>>,
}

/// Milestone 11: collects every `<script>` tag's inline source, in
/// document order — external `src="..."` scripts are explicitly out
/// of scope (would need to thread `network` fetching into this walk
/// too; the mechanism already exists via Milestone 8, just not wired
/// here — a reasonable next increment, not done now).
fn collect_scripts(element: dom::ElementRef<'_>, out: &mut Vec<String>) {
    if element.is(dom::atoms::SCRIPT) {
        for child in element.children() {
            if let dom::ChildRef::Text(text) = child {
                out.push(text.to_string());
            }
        }
    }
    for child in element.children() {
        if let dom::ChildRef::Element(el) = child {
            collect_scripts(el, out);
        }
    }
}

/// `root`'s `<body>` element, if it has one — `None` for a document
/// missing one entirely (malformed input, or a navigated-to page that
/// isn't real HTML).
fn find_body(root: dom::ElementRef<'_>) -> Option<dom::ElementRef<'_>> {
    root.children().find_map(|n| match n {
        dom::ChildRef::Element(el) if el.is(dom::atoms::BODY) => Some(el),
        _ => None,
    })
}

/// Runs every `<script>` tag's source once, in document order —
/// synchronous execution (no async/defer), matching this project's
/// explicit scope since Milestone 11. Shared by `Page::new` (the
/// first page) and `Page::replace_with_navigation` (every page after
/// it) so the two don't drift.
fn run_scripts(document: &Rc<RefCell<dom::Document>>, env: &mut script::ScriptEnvironment) {
    // Collected before any runs (and the borrow released: scripts mutate
    // the document).
    let mut scripts = Vec::new();
    collect_scripts(document.borrow().root(), &mut scripts);
    for script_src in &scripts {
        if let Err(e) = env.run_script(script_src) {
            eprintln!("script error: {e}");
        }
    }
}

/// A document's identity for policy (Milestone 28): the origin of `url`, or
/// a fresh opaque origin for a document with no URL (the built-in demo) —
/// never "no origin".
fn page_identity(url: Option<&str>) -> network::Initiator {
    match url {
        Some(url) => network::Initiator::for_url(url),
        None => network::Initiator::document(network::Origin::new_opaque(), None),
    }
}

/// A new script environment for the document `page` identifies, handed only
/// what `grants` allow, narrowed to that document: storage to its own
/// storage key (ADR 010 §3).
fn script_environment(
    document: &Rc<RefCell<dom::Document>>,
    page: network::Initiator,
    grants: &capability::PageGrants,
    fingerprint: privacy::FingerprintPolicy,
    network: &network::NetworkContext,
) -> script::ScriptEnvironment {
    let key = origin::StorageKey::first_party(&page.origin);
    let capabilities = script::ScriptCapabilities {
        network: grants.network.clone(),
        local_storage: grants.storage.as_ref().map(|s| s.scope(&key)),
        fingerprint,
    };
    let env = js::new_environment(Rc::clone(document), page, capabilities);
    // Milestone 38b (ADR 020): script responses wake the page.
    if let Some(notify) = network.notifier() {
        env.set_wake(std::sync::Arc::clone(notify));
    }
    env
}

impl Page {
    /// M36's gap: takes `new`'s kinds as the page's grants of those kinds
    /// (ADR 010 §4), and hands them to the document showing now — storage
    /// narrowed to its own storage key, as a new document's would be.
    pub(crate) fn grant(&mut self, new: capability::PageGrants) {
        let mut env = self.js_env.borrow_mut();
        let key = origin::StorageKey::first_party(env.origin());
        env.grant(new.network.clone(), new.storage.as_ref().map(|s| s.scope(&key)));
        drop(env);
        self.grants.merge(new);
    }

    /// Builds the FIRST page's state — called once, from `main()`.
    /// `Err` if `root` has no `<body>` (nothing to render): the caller
    /// reports it and exits cleanly — it used to panic, which is the wrong
    /// way to say "the page you asked for is not a renderable document".
    /// A navigated-to page with no `<body>` is the recoverable case — see
    /// `replace_with_navigation`, which leaves the OLD page untouched.
    #[cfg(test)]
    pub fn new(
        root: dom::Element,
        stylesheet: css_parser::Stylesheet,
        image_cache: images::ImageCache,
        url: Option<String>,
        network: network::NetworkContext,
    ) -> Result<Self, String> {
        // Tests: the network and a private store, as a profile would grant.
        let authority = capability::Authority::new();
        let grants = capability::PageGrants { network: Some(authority.network(network.clone())), storage: Some(authority.storage(storage::StorageStore::new())) };
        Self::new_in_profile(root, stylesheet, image_cache, url, network, grants, privacy::FingerprintPolicy::standard())
    }

    /// An empty document with an opaque origin — what a new page shows
    /// until something is loaded into it.
    pub fn blank(network: network::NetworkContext, grants: capability::PageGrants, fingerprint: privacy::FingerprintPolicy) -> Self {
        let mut root = dom::Element::new("html");
        root.append_child(dom::Element::new("body"));
        Self::new_in_profile(root, css_parser::Stylesheet::default(), images::ImageCache::new(), None, network, grants, fingerprint)
            .expect("a document built with a <body> has a <body>")
    }

    /// Like [`Page::new`], with the page's grants (Milestone 36).
    pub fn new_in_profile(
        root: dom::Element,
        stylesheet: css_parser::Stylesheet,
        image_cache: images::ImageCache,
        url: Option<String>,
        network: network::NetworkContext,
        grants: capability::PageGrants,
        fingerprint: privacy::FingerprintPolicy,
    ) -> Result<Self, String> {
        find_body((&root).into()).ok_or_else(|| "the document has no <body> element".to_string())?;
        // Milestone 33: the document is the whole `<html>` element — so
        // `:root` and `html { … }` rules apply and `<body>` inherits from
        // it. (`<head>` is never laid out.)
        // The parsed tree moves into the document — no copy.
        let document = Rc::new(RefCell::new(dom::Document::new(root)));
        let js_env = Rc::new(RefCell::new(script_environment(&document, page_identity(url.as_deref()), &grants, fingerprint, &network)));
        run_scripts(&document, &mut js_env.borrow_mut());
        js_env.borrow_mut().take_dirty(); // pre-render mutations are already reflected in `document` — nothing to signal yet

        Ok(Page {
            document,
            stylesheet: Rc::new(RefCell::new(stylesheet.with_user_agent(style::user_agent_stylesheet()))),
            image_cache: Rc::new(RefCell::new(image_cache)),
            current_url: Rc::new(RefCell::new(url)),
            js_env,
            transition_registry: Rc::new(RefCell::new(style::TransitionRegistry::new())),
            animation_registry: Rc::new(RefCell::new(style::AnimationRegistry::new())),
            last_layout: Rc::new(RefCell::new(None)),
            styles: Rc::new(RefCell::new(style::StyleEngine::new())),
            sheet_generation: Rc::new(Cell::new(1)),
            last_frame: Rc::new(RefCell::new(None)),
            layout_reuse: Rc::new(RefCell::new(layout::LayoutReuse::new())),
            scroll_offset_y: Rc::new(Cell::new(0.0)),
            container_scrolls: Rc::new(RefCell::new(layout::ScrollOffsets::new())),
            pointer: Rc::new(Cell::new(None)),
            hover_path: Rc::default(),
            active_path: Rc::default(),
            pending_nav: Rc::new(RefCell::new(None)),
            network,
            grants,
            fingerprint,
            image_loading: Rc::new(Cell::new(ImageLoading::default())),
            pending_images: Rc::new(RefCell::new(Vec::new())),
            image_dirty: Rc::new(RefCell::new(Vec::new())),
            web_fonts: Rc::new(RefCell::new(Vec::new())),
            ready_fonts: Rc::new(RefCell::new(Vec::new())),
            document_generation: Rc::new(Cell::new(0)),
        })
    }

    /// Moves this document's web fonts along; returns whether any is ready.
    pub fn poll_fonts(&self) -> bool {
        let ready = crate::webfonts::poll(&mut self.web_fonts.borrow_mut(), self.network.notifier());
        let any = !ready.is_empty();
        self.ready_fonts.borrow_mut().extend(ready);
        any
    }

    /// Files the images that have arrived since the last call (decoded in
    /// parallel) and marks the elements showing them for layout. Returns
    /// whether any arrived.
    pub fn poll_images(&self) -> bool {
        let arrived = {
            let mut pending = self.pending_images.borrow_mut();
            if pending.is_empty() {
                return false;
            }
            take_arrived_images(&mut pending, &mut self.image_cache.borrow_mut(), self.network.notifier())
        };
        if arrived.is_empty() {
            return false;
        }
        let arrived: std::collections::HashSet<&str> = arrived.iter().map(String::as_str).collect();
        // One walk for everything that arrived this tick: `<img>`s by
        // `src`, backgrounds by their cascaded image.
        let document = self.document.borrow();
        let styles = self.styles.borrow();
        let mut dirty = self.image_dirty.borrow_mut();
        let mut stack = vec![document.root()];
        while let Some(el) = stack.pop() {
            let shows = (el.is(dom::atoms::IMG) && el.attr_atom(dom::atoms::SRC).is_some_and(|src| arrived.contains(src)))
                || styles.styles().get(&el.node_id()).and_then(|s| s.background_image_path.as_deref()).is_some_and(|p| arrived.contains(p));
            if shows {
                dirty.push(el.node_id());
            }
            stack.extend(el.children().filter_map(|c| match c {
                dom::ChildRef::Element(e) => Some(e),
                dom::ChildRef::Text(_) => None,
            }));
        }
        true
    }

    /// Tears down and rebuilds this page's state IN PLACE for a
    /// successful navigation — every `Page` clone any closure already
    /// holds sees the swap for free (same `Rc`s, new contents), with
    /// no re-wiring needed. Mirrors `Page::new`'s construction exactly
    /// (fresh `JsEnvironment` scoped to the new URL, scripts re-run),
    /// except: `Err(())` on a bodyless navigated-to document is a
    /// RECOVERABLE failure — the caller logs it and the OLD page is
    /// left completely untouched, unlike `Page::new`'s panic at
    /// startup.
    pub fn replace_with_navigation(
        &self,
        new_root: dom::Element,
        new_stylesheet: css_parser::Stylesheet,
        new_image_cache: images::ImageCache,
        pending_images: Vec<PendingImage>,
        requested_url: Option<String>,
    ) -> Result<(), ()> {
        if find_body((&new_root).into()).is_none() {
            return Err(());
        }
        *self.document.borrow_mut() = dom::Document::new(new_root);
        *self.stylesheet.borrow_mut() = new_stylesheet.with_user_agent(style::user_agent_stylesheet());
        *self.image_cache.borrow_mut() = new_image_cache;
        // The old document's images in flight are for nothing now.
        *self.pending_images.borrow_mut() = pending_images;
        self.image_dirty.borrow_mut().clear();
        self.scroll_offset_y.set(0.0);
        self.container_scrolls.borrow_mut().clear();
        // The OLD page's elements are gone — without this, an entry
        // left mid-flight at navigation time would never be revisited
        // (nothing walks the old tree again) and `any_active()` would
        // stay true forever, pinning the redraw loop busy permanently.
        *self.transition_registry.borrow_mut() = style::TransitionRegistry::new();
        *self.animation_registry.borrow_mut() = style::AnimationRegistry::new();
        // The old layout describes the OLD document: a click landing
        // before the next frame must not hit-test against it. (Since M31
        // its keys are node ids, which never name an element of the new
        // document, so this is tidiness, not the security fix it was when
        // the keys were memory addresses.)
        *self.last_layout.borrow_mut() = None;
        // A new document and stylesheet: nothing from the old page's styles
        // or frames carries over.
        *self.styles.borrow_mut() = style::StyleEngine::new();
        self.sheet_generation.set(self.sheet_generation.get() + 1);
        *self.last_frame.borrow_mut() = None;
        *self.layout_reuse.borrow_mut() = layout::LayoutReuse::new();
        *self.current_url.borrow_mut() = requested_url.clone();
        // Milestone 35: the new document's web fonts. Before its first
        // frame when images are waited for too (deterministic loads).
        self.document_generation.set(self.document_generation.get() + 1);
        self.ready_fonts.borrow_mut().clear();
        let loads = crate::webfonts::start(&self.stylesheet.borrow(), requested_url.as_deref(), &self.network);
        if self.image_loading.get() == ImageLoading::BeforeFirstFrame {
            // Inside a tick: kept well under the unresponsive bound (ADR 013).
            *self.ready_fonts.borrow_mut() = crate::webfonts::wait(loads, std::time::Instant::now() + IN_TICK_FONT_WAIT);
            self.web_fonts.borrow_mut().clear();
        } else {
            *self.web_fonts.borrow_mut() = loads;
        }
        // A genuinely fresh environment — the OLD one's globals/
        // listeners/focus/cursor state all belonged to the page that's
        // now gone. Built on the SAME `document` Rc, which now holds
        // the NEW page's body — every closure holding that Rc sees the
        // swap for free.
        *self.js_env.borrow_mut() = script_environment(&self.document, page_identity(requested_url.as_deref()), &self.grants, self.fingerprint, &self.network);
        // The old page's script objects are garbage now: free them.
        js::collect_garbage();
        run_scripts(&self.document, &mut self.js_env.borrow_mut());
        self.js_env.borrow_mut().take_dirty();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use std::time::{Duration, Instant};

    /// The host's demo page (`app/src/demo.html`) — the real document the
    /// desktop app shows, used here so these tests exercise real content.
    const HARDCODED_HTML: &str = include_str!("../../../app/src/demo.html");
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// (status, response headers, body) — what a mock route handler returns.
    type MockResponse = (u16, Vec<(String, String)>, Vec<u8>);

    /// A mock-server handler that also sees the request's headers.
    type HeaderAwareHandler = dyn Fn(&str, &[(String, String)]) -> MockResponse + Send + Sync;

    fn png_fixture_bytes() -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/tests/fixtures/quadrants.png");
        std::fs::read(path).expect("quadrants.png fixture should exist")
    }

    // ---- Pure helpers ----

    #[test]
    fn rewrite_img_srcs_resolves_relative_and_absolute_and_skips_data_uris() {
        let html = r#"<html><body>
            <img src="/logo.png">
            <img src="sub/pic.jpg">
            <img src="https://cdn.example.net/x.png">
            <img src="data:image/png;base64,AAAA">
            <img>
        </body></html>"#;
        let mut root = html_parser::parse_document(html).unwrap();
        let mut urls = Vec::new();
        rewrite_img_srcs_to_absolute(&mut root, "https://example.com/dir/page.html", &mut urls);

        assert_eq!(
            urls,
            vec![
                "https://example.com/logo.png".to_string(),
                "https://example.com/dir/sub/pic.jpg".to_string(),
                "https://cdn.example.net/x.png".to_string(),
            ]
        );

        // The DOM attributes were rewritten in place.
        let mut srcs = Vec::new();
        collect_img_srcs(&root, &mut srcs);
        assert_eq!(srcs[0], "https://example.com/logo.png");
        assert_eq!(srcs[1], "https://example.com/dir/sub/pic.jpg");
        assert_eq!(srcs[3], "data:image/png;base64,AAAA"); // untouched
    }

    fn collect_img_srcs(el: &dom::Element, out: &mut Vec<String>) {
        if el.tag == "img" {
            if let Some(s) = el.attr_atom(dom::atoms::SRC) {
                out.push(s.to_string());
            }
        }
        for c in &el.children {
            if let dom::Node::Element(e) = c {
                collect_img_srcs(e, out);
            }
        }
    }

    #[test]
    fn rewrite_css_urls_handles_quotes_whitespace_and_data_uris() {
        let css = r#"
            .a { background-image: url(images/a.png); }
            .b { background-image: url( "b.png" ); }
            .c { background-image: url('/c.png'); }
            .d { background-image: url(data:image/gif;base64,AAAA); }
        "#;
        let mut urls = Vec::new();
        let rewritten = rewrite_css_image_urls(css, "https://example.com/style/main.css", &mut urls);

        assert_eq!(
            urls,
            vec![
                "https://example.com/style/images/a.png".to_string(),
                "https://example.com/style/b.png".to_string(),
                "https://example.com/c.png".to_string(),
            ]
        );
        assert!(rewritten.contains("url(https://example.com/style/images/a.png)"));
        assert!(rewritten.contains("url(https://example.com/c.png)"));
        assert!(rewritten.contains("url(data:image/gif;base64,AAAA)")); // untouched
    }

    #[test]
    fn only_background_images_are_fetched_not_fonts_cursors_or_masks() {
        let css = "@font-face { font-family: x; src: url(/f.woff2) format('woff2'); }\n\
                   .a { cursor: url(/c.cur), auto; mask-image: url(/m.svg); }\n\
                   .b { background: #fff url(/bg.png) no-repeat; }\n.c{background-image:url(i.jpg)}";
        let mut urls = Vec::new();
        let rewritten = rewrite_css_image_urls(css, "https://example.com/css/s.css", &mut urls);
        assert_eq!(urls, vec!["https://example.com/bg.png".to_string(), "https://example.com/css/i.jpg".to_string()]);
        assert!(rewritten.contains("url(https://example.com/f.woff2)"), "still made absolute");
    }

    #[test]
    fn rewrite_css_urls_leaves_malformed_input_untouched_without_panicking() {
        let css = ".x { background-image: url(no-close ";
        let mut urls = Vec::new();
        let out = rewrite_css_image_urls(css, "https://example.com/", &mut urls);
        assert_eq!(out, css);
        assert!(urls.is_empty());
    }

    #[test]
    fn css_url_rewrite_feeds_an_absolute_background_image_path_into_style() {
        // The point of rewriting the CSS text (not just collecting URLs):
        // `style` must end up with the SAME absolute key the cache is
        // populated under, or layout's lookup misses.
        let css = ".hero { background-image: url(bg.png); }";
        let mut urls = Vec::new();
        let rewritten = rewrite_css_image_urls(css, "https://example.com/a/b.html", &mut urls);
        let sheet = css_parser::parse_stylesheet(&rewritten);

        let mut el = dom::Element::new("div");
        el.set_attr("class", "hero");
        let style = style::resolve_style(&el, &[], None, &sheet);
        assert_eq!(style.background_image_path.as_deref(), Some("https://example.com/a/bg.png"));
        assert_eq!(urls, vec!["https://example.com/a/bg.png".to_string()]);
    }

    // ---- End-to-end against a real local HTTP server ----

    /// Minimal hand-rolled HTTP/1.1 server, same approach as the
    /// `network` crate's own tests — no extra dependency, one
    /// OS-assigned port, routes by path.
    fn spawn_server<F>(handler: F) -> String
    where
        F: Fn(&str) -> MockResponse + Send + Sync + 'static,
    {
        spawn_server_with_headers(move |path, _headers| handler(path))
    }

    /// Like `spawn_server`, but the handler also sees the request's
    /// headers — for tests that assert what the client actually sent.
    fn spawn_server_with_headers<F>(handler: F) -> String
    where
        F: Fn(&str, &[(String, String)]) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = serve_one(stream, handler.as_ref());
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn serve_one(
        stream: TcpStream,
        handler: &HeaderAwareHandler,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let mut request_headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim_end().is_empty() {
                break;
            }
            if let Some((k, v)) = line.trim_end().split_once(':') {
                request_headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let (status, headers, body) = handler(&path, &request_headers);
        let mut stream = stream;
        write!(stream, "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\n", body.len())?;
        for (k, v) in &headers {
            write!(stream, "{k}: {v}\r\n")?;
        }
        write!(stream, "Connection: close\r\n\r\n")?;
        stream.write_all(&body)?;
        stream.flush()
    }

    #[test]
    fn fetch_page_loads_linked_css_and_a_relative_img_end_to_end() {
        let png = png_fixture_bytes();
        let png_for_handler = png.clone();
        let base = spawn_server(move |path| match path {
            "/" | "/index.html" => (
                200,
                vec![("Content-Type".into(), "text/html".into())],
                br#"<html><head><link rel="stylesheet" href="/style.css"></head>
                    <body><img src="assets/photo.png"><div class="hero">hi</div></body></html>"#
                    .to_vec(),
            ),
            "/style.css" => (
                200,
                vec![("Content-Type".into(), "text/css".into())],
                b".hero { background-image: url(/bg.png); width: 100px; height: 40px; }".to_vec(),
            ),
            "/assets/photo.png" | "/bg.png" => {
                (200, vec![("Content-Type".into(), "image/png".into())], png_for_handler.clone())
            }
            _ => (404, vec![], b"nope".to_vec()),
        });

        let mut cache = images::ImageCache::new();
        let (root, stylesheet) = fetch_page(&format!("{base}/index.html"), &mut cache, &network::NetworkContext::new(), None).expect("page should load");

        // Relative <img src> was rewritten to absolute and its bytes cached.
        assert!(cache.contains(&format!("{base}/assets/photo.png")), "relative <img> should be fetched + cached");
        // background-image url(/bg.png) was fetched + cached.
        assert!(cache.contains(&format!("{base}/bg.png")), "background-image should be fetched + cached");

        // The full pipeline now paints both: one <img> box, one
        // background-image box -> two distinct decoded image resources.
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(256, 128);
        let laid_out = layout::layout_document(&root, 800.0, &font, &stylesheet, &mut cache);
        let (_items, image_resources) = rendering::display_list::build_display_list(&laid_out, &font, &mut atlas);
        assert_eq!(image_resources.len(), 2, "expected the <img> and the background-image to both decode");
    }

    #[test]
    fn a_page_load_waits_for_its_images_all_together_and_decodes_what_arrived() {
        // Performance audit: each resource had its own 10 s deadline, so
        // N slow images waited N times as long; and images decoded one after
        // another. Now one deadline covers them all.
        let base = spawn_server(move |path| match path {
            "/p.html" => (200, vec![], b"<html><body><img src=\"/a.png\"><img src=\"/slow1.png\"><img src=\"/slow2.png\"><img src=\"/slow3.png\"></body></html>".to_vec()),
            "/a.png" => (200, vec![], png_fixture_bytes()),
            _ => {
                std::thread::sleep(Duration::from_secs(3));
                (200, vec![], png_fixture_bytes())
            }
        });
        let mut cache = images::ImageCache::new();
        let (_, _, pending) = fetch_page_progressive(&format!("{base}/p.html"), &mut cache, &network::NetworkContext::new(), None).unwrap();
        assert_eq!(pending.len(), 4);
        assert!(pending.iter().all(|p| cache.is_pending(&p.url)), "in flight: never looked for on disk");
        let started = Instant::now();
        wait_for_images_until(pending, &mut cache, Instant::now() + Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_millis(1500), "one deadline for all of them: waited {:?}", started.elapsed());
        assert!(cache.contains(&format!("{base}/a.png")), "what arrived in time is decoded");
        assert!(!cache.contains(&format!("{base}/slow1.png")));
    }

    #[test]
    fn fetch_page_degrades_when_an_image_404s_rather_than_failing_the_load() {
        let base = spawn_server(move |path| match path {
            "/p.html" => (
                200,
                vec![],
                b"<html><body><img src=\"/missing.png\"><p>text still renders</p></body></html>".to_vec(),
            ),
            _ => (404, vec![], b"not found".to_vec()),
        });

        let mut cache = images::ImageCache::new();
        let (root, stylesheet) = fetch_page(&format!("{base}/p.html"), &mut cache, &network::NetworkContext::new(), None).expect("page should still load");

        // A 404 body is not a decodable image — nothing cached, no panic.
        assert!(!cache.contains(&format!("{base}/missing.png")));

        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(256, 128);
        let laid_out = layout::layout_document(&root, 800.0, &font, &stylesheet, &mut cache);
        let (_items, image_resources) = rendering::display_list::build_display_list(&laid_out, &font, &mut atlas);
        assert!(image_resources.is_empty(), "a broken image must not produce a resource");
    }

    // ---- Milestone 21: page navigation ----

    #[test]
    fn navigate_async_loads_a_real_page_on_a_background_thread_without_blocking() {
        let base = spawn_server(move |path| match path {
            "/" | "/index.html" => (
                200,
                vec![],
                b"<html><body><h1 id=\"h\">navigated page</h1></body></html>".to_vec(),
            ),
            _ => (404, vec![], b"nope".to_vec()),
        });

        let start = Instant::now();
        let handle = navigate_async(&format!("{base}/index.html"), network::NetworkContext::new(), None);
        // The call above must return immediately — it dispatches to a
        // background thread, it doesn't fetch inline. A real assertion,
        // not just "it didn't hang": if this were still synchronous,
        // it would have already paid the real network round-trip by
        // the time we get here.
        assert!(start.elapsed() < Duration::from_millis(50), "navigate_async must return immediately, not block on the fetch");

        let (root, _stylesheet, _cache, _) = poll_until_settled_navigation(&handle).expect("navigation should complete").expect("page should load");
        assert_eq!(root.find_by_id("h").map(|el| el.text_content()), Some("navigated page".to_string()));
    }

    #[test]
    fn navigate_async_reports_an_error_for_an_unroutable_url_without_panicking() {
        let handle = navigate_async("http://127.0.0.1:1/nope", network::NetworkContext::new(), None);
        let result = poll_until_settled_navigation(&handle);
        assert!(result.is_some(), "should eventually settle, not hang forever");
        assert!(result.unwrap().is_err());
    }

    /// Test-only: blocks (bounded) waiting for a `NavigationHandle` to
    /// settle — production code never blocks like this (`app`'s `tick`
    /// polls once per frame instead), this is purely to make the test
    /// itself deterministic without a real event loop.
    fn poll_until_settled_navigation(handle: &NavigationHandle) -> Option<NavigationResult> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(result) = handle.try_recv() {
                return Some(result);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    // ---- Milestone 23: classic (non-JS) form navigation, end to end ----

    /// Milestone 21 already proves the TRIGGER logic (`js::dispatch_click`
    /// building the right query string) and the FETCH mechanism
    /// (`navigate_async` against a real server) separately. This
    /// milestone's own acceptance criterion asks for the chain between
    /// them, exercised together for the first time: a real classic GET
    /// form submit's query string, resolved against a base URL via
    /// `network::resolve_url` (the same resolution `main.rs`'s
    /// `resolve_navigation_target` uses — that function itself lives in
    /// the `main.rs` binary target, not this library, so isn't directly
    /// callable from here; this test exercises the same real
    /// `network::resolve_url` call it delegates to), fetched against a
    /// REAL local server, and confirmed to have reached the exact path
    /// the form's own fields constructed — not a hand-typed URL.
    fn full_hit_path_to(root: dom::ElementRef<'_>, id: &str) -> Vec<dom::NodeId> {
        fn walk(el: dom::ElementRef<'_>, id: &str, path: &mut Vec<dom::NodeId>) -> bool {
            if el.attr_atom(dom::atoms::ID) == Some(id) {
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
        let mut path = Vec::new();
        walk(root, id, &mut path);
        path
    }

    #[test]
    fn classic_get_form_submission_navigates_end_to_end_against_a_real_server() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let base = spawn_server(|path| match path {
            "/search?q=hello+world" => (
                200,
                vec![],
                b"<html><body><h1 id=\"result\">found it</h1></body></html>".to_vec(),
            ),
            _ => (404, vec![], b"wrong query string reached the server".to_vec()),
        });

        let mut root = dom::Element::new("div");
        let mut form = dom::Element::new("form");
        form.attributes.push(("id".into(), "f".into()));
        form.attributes.push(("action".into(), "/search".into()));
        let mut q = dom::Element::new("input");
        q.attributes.push(("name".into(), "q".into()));
        q.attributes.push(("value".into(), "hello world".into()));
        let mut button = dom::Element::new("button");
        button.attributes.push(("id".into(), "go".into()));
        form.children.push(dom::Node::Element(q));
        form.children.push(dom::Node::Element(button));
        root.children.push(dom::Node::Element(form));
        let doc = Rc::new(RefCell::new(dom::Document::new(root)));

        let mut env = js::JsEnvironment::new(Rc::clone(&doc));
        let hit_path = full_hit_path_to(doc.borrow().root(), "go");
        env.dispatch_click(&hit_path);

        let (action, query) = env.take_pending_navigation().expect("classic GET form submit should set a pending navigation");
        let target = if query.is_empty() { action } else { format!("{action}?{query}") };
        let resolved = network::resolve_url(&format!("{base}/"), &target).expect("action should resolve against the page's base URL");

        let handle = navigate_async(&resolved, network::NetworkContext::new(), None);
        let (new_root, _stylesheet, _cache, _) =
            poll_until_settled_navigation(&handle).expect("navigation should complete").expect("the server's real response should load correctly");
        assert_eq!(
            new_root.find_by_id("result").map(|el| el.text_content()),
            Some("found it".to_string()),
            "the query string the classic form actually built must be the one that reached the server"
        );
    }

    // ---- Milestone 24: accessibility tree, against the REAL demo document ----

    /// Not wired into `main.rs`'s render loop yet (there's no consumer
    /// for the tree until a real platform AT integration exists — see
    /// `milestones/milestone-24-accessibility-tree.md`'s "Out of
    /// scope" section) — this proves `accessibility::build_tree`
    /// handles `app`'s actual, real demo HTML correctly, not just the
    /// synthetic fixtures `accessibility`'s own test suite otherwise
    /// covers.
    #[test]
    fn accessibility_tree_from_the_real_demo_html_has_correct_roles_and_names() {
        let root = html_parser::parse_document(HARDCODED_HTML).expect("the real demo HTML must parse");
        let body = root
            .children
            .iter()
            .find_map(|n| match n {
                dom::Node::Element(el) if el.tag == "body" => Some(el),
                _ => None,
            })
            .expect("demo document should have a body");

        let tree = accessibility::build_tree(body, Some("search"));

        fn find_by_id<'a>(node: &'a accessibility::AccessibleNode, target_id: &str, el: &'a dom::Element) -> Option<(&'a accessibility::AccessibleNode, &'a dom::Element)> {
            if el.attr_atom(dom::atoms::ID) == Some(target_id) {
                return Some((node, el));
            }
            let child_els: Vec<&dom::Element> = el
                .children
                .iter()
                .filter_map(|c| match c {
                    dom::Node::Element(e) => Some(e),
                    dom::Node::Text(_) => None,
                })
                .collect();
            for (child_node, child_el) in node.children.iter().zip(child_els) {
                if let Some(found) = find_by_id(child_node, target_id, child_el) {
                    return Some(found);
                }
            }
            None
        }

        let (search, _) = find_by_id(&tree, "search", body).expect("the real search input should be in the tree");
        assert_eq!(search.role, accessibility::Role::TextBox);
        assert!(search.state.focused);
        assert!(search.state.required);

        let (submit, _) = find_by_id(&tree, "search-submit", body).expect("the real submit button should be in the tree");
        assert_eq!(submit.role, accessibility::Role::Button);
        assert_eq!(submit.name, Some("Search".to_string()));

        let (agree, _) = find_by_id(&tree, "agree", body).expect("the real checkbox should be in the tree");
        assert_eq!(agree.role, accessibility::Role::CheckBox);
        assert_eq!(agree.state.checked, Some(false));

        let (link, _) = find_by_id(&tree, "nav-link", body).expect("the real navigation link should be in the tree");
        assert_eq!(link.role, accessibility::Role::Link);
        assert_eq!(link.name, Some("a real external link".to_string()));

        let (note, _) = find_by_id(&tree, "editable-note", body).expect("the real contenteditable note should be in the tree");
        assert_eq!(note.role, accessibility::Role::TextBox, "contenteditable must get a text box role, not generic");
    }

    // ---- Real-bug regression: clicking the demo's actual nav link ----

    fn find_box_by_element_key<'a>(node: &'a layout::LayoutBox, target_key: dom::NodeId) -> Option<&'a layout::LayoutBox> {
        if node.element_key == Some(target_key) {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_box_by_element_key(c, target_key))
    }

    /// Reproduces a real bug a human found by actually clicking the
    /// running app: the Milestone 21 demo's own prose says "clicking
    /// this replaces this ENTIRE page... a real external link" — but
    /// clicking that rendered link text did NOTHING. Root cause: every
    /// inline-text word box's `element_key` was unconditionally
    /// `None` (see `layout::InlineWord::element_key`'s doc comment for
    /// the full story), so `layout::hit_test_path` could never name an
    /// `<a>` as a click target at all when it was reached through its
    /// own rendered text (the overwhelmingly common way a real link is
    /// clicked) — only a click on some OTHER hit-testable box (there
    /// isn't one here) would have worked. This test lays out the REAL
    /// demo HTML exactly like `app`'s own render loop does, finds the
    /// real on-screen position of the real `<a id="nav-link">`, and
    /// dispatches a click there through the REAL `layout::hit_test_path`
    /// → `js::dispatch_click` pipeline (not a synthetic hand-built hit
    /// path, unlike this file's other click-dispatch tests) — proving
    /// the exact end-to-end path a human's mouse click takes.
    #[test]
    fn clicking_the_real_demo_nav_links_own_rendered_text_triggers_navigation() {
        let root = html_parser::parse_document(HARDCODED_HTML).expect("the real demo HTML must parse");
        let body = root
            .children
            .iter()
            .find_map(|n| match n {
                dom::Node::Element(el) if el.tag == "body" => Some(el.clone()),
                _ => None,
            })
            .expect("demo document should have a body");

        let font = text::Font::dejavu_sans();
        let mut image_cache = images::ImageCache::new();
        let laid_out = layout::layout_document(&body, 900.0, &font, &css_parser::Stylesheet::default(), &mut image_cache);

        let link_key = body.find_by_id("nav-link").map(|e| e.node_id).expect("the real nav-link element should exist");
        let link_box = find_box_by_element_key(&laid_out, link_key).expect("the link must have its OWN hit-testable box now, not just its containing paragraph");

        let click_x = link_box.x + 1.0;
        let click_y = link_box.y + 1.0;
        let hit_path = layout::hit_test_path(&laid_out, click_x, click_y);
        assert_eq!(hit_path.first(), Some(&link_key), "a click on the link's own rendered text must resolve to the <a>, not just the paragraph");

        let doc = std::rc::Rc::new(std::cell::RefCell::new(dom::Document::new(body)));
        let mut env = js::JsEnvironment::new(std::rc::Rc::clone(&doc));
        env.dispatch_click(&hit_path);

        let (href, _query) = env.take_pending_navigation().expect("clicking the real link's real rendered text must trigger navigation");
        assert_eq!(href, "https://example.com/");
    }

    // ---- Milestone 26: one shared network context, end to end ----

    #[test]
    fn a_cookie_set_by_the_page_load_is_sent_by_the_pages_own_script_fetch() {
        // The bug this milestone fixes, through the REAL path: the page
        // load (`fetch_page`) and the page's JS `fetch()` used to build
        // separate `DirectRoute`s, so the session cookie the server set
        // on the page response never reached the page's own API call.
        let api_cookie: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let base_cell: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));

        let (seen, base_for_handler) = (Arc::clone(&api_cookie), Arc::clone(&base_cell));
        let base = spawn_server_with_headers(move |path, headers| match path {
            "/page.html" => {
                let base = base_for_handler.lock().unwrap().clone();
                let html = format!(
                    "<html><body><p id=\"out\">waiting</p><script>\
                       fetch('{base}/api').then(function (r) {{ return r.text(); }})\
                         .then(function (t) {{ document.getElementById('out').textContent = t; }});\
                     </script></body></html>"
                );
                (200, vec![("Set-Cookie".to_string(), "sid=from-page-load; Path=/".to_string())], html.into_bytes())
            }
            "/api" => {
                *seen.lock().unwrap() =
                    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.clone());
                (200, vec![], b"api-ok".to_vec())
            }
            _ => (404, vec![], vec![]),
        });
        *base_cell.lock().unwrap() = base.clone();

        let network = network::NetworkContext::new();
        let url = format!("{base}/page.html");
        let mut cache = images::ImageCache::new();
        let (root, stylesheet) = fetch_page(&url, &mut cache, &network, None).expect("page should load");
        let page = Page::new(root, stylesheet, cache, Some(url), network.clone()).expect("test page has a body");

        // Drive the page's fetch to completion, the way the tick loop does.
        let deadline = Instant::now() + Duration::from_secs(5);
        while page.js_env.borrow().has_pending_fetches() {
            assert!(Instant::now() < deadline, "the page's fetch never settled");
            std::thread::sleep(Duration::from_millis(5));
            page.js_env.borrow_mut().poll_pending_fetches();
        }
        page.js_env.borrow_mut().run_script("0").unwrap(); // drain the promise jobs

        assert_eq!(
            api_cookie.lock().unwrap().as_deref(),
            Some("sid=from-page-load"),
            "the cookie set by the page load must be sent by the page's own fetch()"
        );
        assert_eq!(page.document.borrow().find_by_id("out").map(|e| e.text_content()), Some("api-ok".to_string()));
    }

    #[test]
    fn navigation_carries_the_profiles_cookies_to_the_next_page() {
        let seen: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_srv = Arc::clone(&seen);
        let base = spawn_server_with_headers(move |path, headers| {
            seen_srv.lock().unwrap().push(headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.clone()));
            let set = if path == "/login" { vec![("Set-Cookie".to_string(), "sid=42; Path=/".to_string())] } else { vec![] };
            (200, set, b"<html><body><p>x</p></body></html>".to_vec())
        });

        let network = network::NetworkContext::new();
        let first = navigate_async(&format!("{base}/login"), network.clone(), None);
        poll_until_settled_navigation(&first).expect("first navigation completes").expect("loads");
        let second = navigate_async(&format!("{base}/account"), network.clone(), None);
        poll_until_settled_navigation(&second).expect("second navigation completes").expect("loads");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.first(), Some(&None), "no cookie on the very first request");
        assert_eq!(seen.last(), Some(&Some("sid=42".to_string())), "the second navigation must carry the cookie the first one set");
    }

    // ---- Milestone 33: the document is <html>; UA sheet; canvas ----

    fn page_from(html: &str) -> Page {
        let root = html_parser::parse_document(html).expect("parses");
        let mut style_text = String::new();
        collect_style_text(dom::ElementRef::from(&root), &mut style_text);
        Page::new(root, css_parser::parse_stylesheet(&style_text), images::ImageCache::new(), None, network::NetworkContext::new()).expect("has a body")
    }

    fn scene_of(page: &Page) -> rendering::Scene {
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);
        frame::build_frame(page, &font, &mut atlas, 400.0, 300.0, Instant::now()).0
    }

    #[test]
    fn the_document_root_is_html_so_root_rules_and_rem_apply() {
        let page = page_from("<html><head><style>:root { font-size: 10px } html { color: #ff0000 } .x { width: 5rem; height: 1px }</style></head><body><div class=x></div></body></html>");
        assert_eq!(page.document.borrow().root().tag(), "html");
        let scene = scene_of(&page);
        let layout = page.last_layout.borrow();
        let body = &layout.as_ref().unwrap().children[0];
        assert_eq!(body.children[0].width, 50.0, "rem follows :root's font-size");
        let styles = page.styles.borrow();
        let body_el = page.document.borrow().root().element_children().find(|e| e.is(dom::atoms::BODY)).map(|e| e.node_id()).unwrap();
        assert_eq!(styles.styles()[&body_el].color, style::Color { r: 255, g: 0, b: 0, a: 255 }, "body inherits from html");
        assert_eq!(scene.background, style::Color { r: 255, g: 255, b: 255, a: 255 });
    }

    #[test]
    fn the_user_agent_sheet_gives_body_its_margin_and_headings_their_size_below_author_rules() {
        let page = page_from("<html><body><h1>Title</h1><p class=a>x</p></body></html>");
        scene_of(&page);
        let layout = page.last_layout.borrow();
        let body = &layout.as_ref().unwrap().children[0];
        assert_eq!((body.x, body.y), (8.0, 8.0), "body {{ margin: 8px }}");
        let page = page_from("<html><head><style>body { margin: 0 } p { margin: 0 }</style></head><body><p>x</p></body></html>");
        scene_of(&page);
        let layout = page.last_layout.borrow();
        let body = &layout.as_ref().unwrap().children[0];
        assert_eq!((body.x, body.y, body.children[0].y), (0.0, 0.0, 0.0), "author rules beat the UA sheet");
    }

    #[test]
    fn the_canvas_takes_the_root_or_else_the_body_background() {
        let bg = |css: &str| scene_of(&page_from(&format!("<html><head><style>{css}</style></head><body>x</body></html>"))).background;
        assert_eq!(bg("body { background-color: #102030 }"), style::Color { r: 0x10, g: 0x20, b: 0x30, a: 255 });
        assert_eq!(bg("html { background-color: #010203 } body { background-color: #102030 }"), style::Color { r: 1, g: 2, b: 3, a: 255 });
        assert_eq!(bg("body { background-color: rgba(0, 0, 0, 0.5) }"), style::Color { r: 127, g: 127, b: 127, a: 255 }, "composited over white");
        assert_eq!(bg(""), style::Color { r: 255, g: 255, b: 255, a: 255 });
    }

    // ---- Milestone 27: a frame of the REAL demo page is traceable ----

    /// Milestone 34 acceptance: a transform-only animation of an element on
    /// a layer of its own is drawn by the compositor — no layout, no display
    /// list build, one layer moved a frame — and draws exactly what a full
    /// build from the same layout would.
    #[test]
    fn a_transform_animation_runs_on_the_compositor_and_matches_a_full_build() {
        let page = page_from(
            "<html><head><style>@keyframes spin { from { transform: rotate(0deg) } to { transform: rotate(360deg) } }\
             #s { width: 50px; height: 50px; background-color: #ff0000; animation: spin 1s linear infinite }\
             p { color: #000000 }</style></head><body><div id=\"s\"><p>x</p></div><p>below</p></body></html>",
        );
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let start = Instant::now();
        let mut frames = Vec::new();
        for ms in [0u64, 100, 200, 300] {
            frames.push(frame::build_frame(&page, &font, &mut atlas, 400.0, 300.0, start + std::time::Duration::from_millis(ms)));
        }
        let angle = |scene: &rendering::Scene| {
            scene.items.iter().find_map(|i| match i {
                rendering::DisplayItem::PushTransform(l) => Some(l.matrix.0[1].atan2(l.matrix.0[0]).to_degrees()),
                _ => None,
            })
        };
        for (n, (scene, stats)) in frames.iter().enumerate().skip(2) {
            assert_eq!((stats.restyled, stats.full_restyle), (0, false), "frame {n}: no cascade");
            assert_eq!(stats.layout, crate::LayoutWork::Reused, "frame {n}: no layout");
            assert!(!stats.display_list_built, "frame {n}: no display list build");
            assert_eq!(stats.composited, 1, "frame {n}: one layer moved");
            let expected = [0.0, 36.0, 72.0, 108.0][n];
            assert!((angle(scene).unwrap() - expected).abs() < 0.01, "frame {n}: {:?}", angle(scene));
        }
        // The in-place update equals building from the (updated) layout.
        let (last, _) = frames.last().unwrap();
        let layout = page.last_layout.borrow();
        let mut atlas2 = text::GlyphAtlas::new(512, 128);
        let (mut built, _, _) = rendering::build_display_list_for_frame(layout.as_ref().unwrap(), &font, &mut atlas2, 0.0, &Default::default());
        rendering::cull_to_viewport(&mut built, 400.0, 300.0);
        let strip = |items: &[rendering::DisplayItem]| {
            // Glyph atlas positions depend on the atlas; the geometry must not.
            items.iter().map(|i| match i {
                rendering::DisplayItem::Glyphs { instances, .. } => format!("glyphs {:?}", instances.iter().map(|g| (g.x, g.y, g.width, g.height)).collect::<Vec<_>>()),
                other => format!("{other:?}"),
            }).collect::<Vec<_>>()
        };
        assert_eq!(strip(&last.items), strip(&built));
    }

    /// Milestone 34: scrolling a scroll container moves its layer — no
    /// layout, no display-list build.
    #[test]
    fn a_container_scroll_is_a_compositor_update() {
        let page = page_from(
            "<html><head><style>#box { overflow: auto; height: 60px } #box p { height: 40px; margin: 0 }</style></head>\
             <body><div id=\"box\"><p>a</p><p>b</p><p>c</p></div></body></html>",
        );
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let now = Instant::now();
        for _ in 0..2 {
            frame::build_frame(&page, &font, &mut atlas, 400.0, 300.0, now);
        }
        let key = page.document.borrow().find_by_id("box").unwrap().node_id();
        page.container_scrolls.borrow_mut().insert(key, (0.0, 25.0));
        let (scene, stats) = frame::build_frame(&page, &font, &mut atlas, 400.0, 300.0, now);
        assert_eq!(stats.layout, crate::LayoutWork::Reused);
        assert!(!stats.display_list_built, "drawn from the page's list");
        assert_eq!(stats.composited, 1, "one scroll layer moved");
        let offset = scene.items.iter().find_map(|i| match i {
            rendering::DisplayItem::PushTransform(l) if l.scroll.is_some() => Some(l.matrix.0[5]),
            _ => None,
        });
        assert_eq!(offset, Some(-25.0));
        assert!(matches!(stats.damage, rendering::Damage::Rect { .. }), "only the container is damaged: {:?}", stats.damage);
    }

    fn demo_page() -> Page {
        let root = html_parser::parse_document(HARDCODED_HTML).expect("demo HTML parses");
        let mut style_text = String::new();
        collect_style_text(dom::ElementRef::from(&root), &mut style_text);
        Page::new(root, css_parser::parse_stylesheet(&style_text), images::ImageCache::new(), None, network::NetworkContext::new()).expect("test page has a body")
    }

    #[test]
    fn a_demo_frame_reports_every_pipeline_stage_as_a_nested_span() {
        // What `app --trace` prints per frame, built headlessly: no
        // window, no GPU.
        let page = demo_page();
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let before = diagnostics::metrics::snapshot();

        let (scene, captured) = diagnostics::collector::capture(|| frame::build_frame(&page, &font, &mut atlas, 800.0, 700.0, Instant::now()).0);

        assert!(!scene.items.is_empty(), "the demo page really rendered something");
        let frame = captured.span("frame").expect("an outermost `frame` span");
        assert_eq!(frame.depth, 0);
        // (Milestone 31: transitions and `@keyframes` are one `animations`
        // stage over just the animated elements.)
        for stage in ["cascade", "animations", "layout", "display_list", "scene_snapshot"] {
            let span = captured.span(stage).unwrap_or_else(|| panic!("missing `{stage}` span; saw {:?}", captured.span_names()));
            assert_eq!(span.depth, 1, "`{stage}` nests directly under `frame`");
            assert!(span.duration <= frame.duration, "`{stage}` cannot take longer than the frame containing it");
        }
        // The stage spans are in pipeline order, not shuffled.
        let order: Vec<&str> = captured.span_names().into_iter().filter(|n| *n != "frame").collect();
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("cascade") < pos("animations"));
        assert!(pos("animations") < pos("layout") && pos("layout") < pos("display_list"));

        // ...and what `--trace` would print.
        let report = captured.report("frame #1");
        if std::env::var_os("SHOW_TRACE").is_some() {
            eprintln!("{report}"); // `SHOW_TRACE=1 cargo test -p app demo_frame -- --nocapture`
        }
        for stage in ["frame", "cascade", "layout", "display_list"] {
            assert!(report.contains(stage), "report lists `{stage}`:\n{report}");
        }
        assert!(report.contains(" ms"), "with timings:\n{report}");

        let after = diagnostics::metrics::snapshot();
        assert!(after.delta(&before, "frames_built") >= 1);
        assert!(after.get("style_cache_entries") > 0, "the cascade gauge is set");
    }

    #[test]
    fn detailed_metrics_count_layout_boxes_only_when_asked() {
        let page = demo_page();
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);

        // Detailed for this thread only: other tests' frames, built in
        // parallel, must not write the gauge between the frame and the read.
        let counted = diagnostics::metrics::detailed_on_this_thread(|| {
            let _ = frame::build_frame(&page, &font, &mut atlas, 800.0, 700.0, Instant::now());
            diagnostics::metrics::LAYOUT_BOX_COUNT.get()
        });
        assert!(counted > 5, "the demo page has many boxes; counted {counted}");

        // With detailed metrics off the gauge is left alone (no tree walk).
        diagnostics::metrics::LAYOUT_BOX_COUNT.set(u64::MAX);
        let _ = frame::build_frame(&page, &font, &mut atlas, 800.0, 700.0, Instant::now());
        let untouched = diagnostics::metrics::LAYOUT_BOX_COUNT.get();
        diagnostics::metrics::LAYOUT_BOX_COUNT.set(0);
        // (Another test may have enabled `detailed` meanwhile; only the
        // "off" case is meaningful to assert, and only if it stayed off.)
        if !diagnostics::metrics::detailed() {
            assert_eq!(untouched, u64::MAX, "no tree walk when detailed metrics are off");
        }
    }

    #[test]
    fn an_untraced_frame_installs_nothing_and_still_renders_identically() {
        // With no subscriber the spans are no-ops: the scene is the same
        // whether or not anyone is listening.
        let page = demo_page();
        let font = text::Font::dejavu_sans();
        let mut atlas_a = text::GlyphAtlas::new(512, 128);
        let mut atlas_b = text::GlyphAtlas::new(512, 128);
        // One clock for both: the page has transitions, and what is drawn
        // must differ only if tracing changed it.
        let now = Instant::now();
        let untraced = frame::build_frame(&page, &font, &mut atlas_a, 800.0, 700.0, now).0;
        let (traced, _) = diagnostics::collector::capture(|| frame::build_frame(&page, &font, &mut atlas_b, 800.0, 700.0, now).0);
        assert_eq!(untraced.items.len(), traced.items.len());
        assert_eq!(format!("{:?}", untraced.items), format!("{:?}", traced.items), "tracing must not change what is drawn");
    }

    #[test]
    fn navigation_emits_start_and_finish_events_with_a_sanitized_url() {
        let base = spawn_server(|_path| (200, vec![], b"<html><body><p>hi</p></body></html>".to_vec()));
        let network = network::NetworkContext::new();
        let (handle, started) = diagnostics::collector::capture(|| navigate_async(&format!("{base}/page?session=SECRET"), network.clone(), None));
        let messages: Vec<&str> = started.events.iter().map(|e| e.message.as_str()).collect();
        assert!(messages.contains(&"navigation started"), "{messages:?}");
        assert!(
            started.events.iter().all(|e| !format!("{:?}", e.fields).contains("SECRET")),
            "the query string must not appear in diagnostics"
        );
        poll_until_settled_navigation(&handle).expect("navigation completes").expect("loads");
    }

    // ---- Hardening pass: a fetched page cannot make the engine read local files ----

    #[test]
    fn a_fetched_page_cannot_make_the_engine_read_a_local_file() {
        // The attack: inline `style` (not rewritten by `fetch_page`) and a
        // relative-looking `<img>` src pointing at LOCAL absolute paths.
        // Before the fix the engine decoded the local PNG.
        let png = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/tests/fixtures/quadrants.png");
        let html = format!(
            "<html><body><div style=\"width:50px;height:50px;background-image:url({png})\">x</div>\
             <img src=\"{png}\"></body></html>"
        );
        let base = spawn_server(move |_| (200, vec![], html.clone().into_bytes()));

        let before = diagnostics::metrics::snapshot();
        let mut cache = images::ImageCache::new();
        let (root, sheet) = fetch_page(&format!("{base}/"), &mut cache, &network::NetworkContext::new(), None).unwrap();
        let font = text::Font::dejavu_sans();
        let laid_out = layout::layout_document(&root, 800.0, &font, &sheet, &mut cache);

        assert_eq!(cache.decode_count(), 0, "no local file may be decoded on a page's behalf");
        assert!(!cache.contains(png), "and none may be cached");
        assert!(laid_out.width > 0.0, "the page still lays out; the images just don't paint");
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.file_access_denied") >= 1, "the refusal is observable");
    }

    #[test]
    fn navigation_builds_content_driven_caches_with_no_file_access() {
        let base = spawn_server(|_| (200, vec![], b"<html><body><p>x</p></body></html>".to_vec()));
        let handle = navigate_async(&format!("{base}/"), network::NetworkContext::new(), None);
        let (_, _, cache, _) = poll_until_settled_navigation(&handle).unwrap().unwrap();
        assert_eq!(cache.file_access(), images::FileAccess::Denied);
    }

    // ---- Hardening pass: cross-site cookie and Referer policy, end to end ----

    type Seen = Arc<std::sync::Mutex<Vec<(String, Option<String>, Option<String>)>>>; // (path, cookie, referer)

    /// A "third-party" server on `localhost` (a different SITE from the
    /// `127.0.0.1` first-party page). `/set` sets one Lax-by-default and
    /// one Strict cookie; every request it receives is recorded.
    fn third_party_server(seen: Seen) -> String {
        let base = spawn_server_with_headers(move |path, headers| {
            let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
            seen.lock().unwrap().push((path.to_string(), get("cookie"), get("referer")));
            let set = if path == "/set" {
                vec![
                    ("Set-Cookie".to_string(), "lax_by_default=1".to_string()),
                    ("Set-Cookie".to_string(), "strict=1; SameSite=Strict".to_string()),
                ]
            } else {
                vec![]
            };
            (200, set, b"<html><body><p>third party</p></body></html>".to_vec())
        });
        base.replace("127.0.0.1", "localhost")
    }

    #[test]
    fn third_party_subresources_and_script_fetches_carry_no_samesite_cookies_and_a_minimal_referer() {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let third = third_party_server(Arc::clone(&seen));
        let third_for_page = third.clone();
        let page_base = spawn_server(move |path| match path {
            "/page.html" => (
                200,
                vec![],
                format!(
                    "<html><body><img src=\"{third_for_page}/pixel.png\"><script>fetch('{third_for_page}/api?token=SECRET');</script></body></html>"
                )
                .into_bytes(),
            ),
            _ => (404, vec![], vec![]),
        });

        let network = network::NetworkContext::new();
        // The user has visited the third party directly, so it has cookies here.
        network.fetch_with(&format!("{third}/set"), &network::RequestContext::typed_navigation()).unwrap();
        assert!(network.cookie_jar().header_for("localhost", "/", true).is_some(), "setup: the third party's cookies are in the jar");

        // Now load a DIFFERENT-site page that embeds the third party.
        let url = format!("{page_base}/page.html");
        let mut cache = images::ImageCache::new();
        let (root, sheet) = fetch_page(&url, &mut cache, &network, None).unwrap();
        let page = Page::new(root, sheet, cache, Some(url), network.clone()).expect("test page has a body");
        let deadline = Instant::now() + Duration::from_secs(5);
        while page.js_env.borrow().has_pending_fetches() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            page.js_env.borrow_mut().poll_pending_fetches();
        }
        // The image request happened during fetch_page; wait for the fetch too.
        std::thread::sleep(Duration::from_millis(100));

        let seen = seen.lock().unwrap().clone();
        for path in ["/pixel.png", "/api?token=SECRET"] {
            let (_, cookie, referer) = seen.iter().find(|(p, _, _)| p == path).unwrap_or_else(|| panic!("{path} was never requested: {seen:?}"));
            assert_eq!(*cookie, None, "{path}: a cross-site subresource must not carry the third party's cookies");
            assert_eq!(referer.as_deref(), Some(format!("{page_base}/").as_str()), "{path}: cross-origin Referer is the origin only, never the path or query");
        }
    }

    #[test]
    fn a_cross_site_link_click_carries_lax_cookies_but_never_strict_ones() {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let third = third_party_server(Arc::clone(&seen));
        let network = network::NetworkContext::new();
        network.fetch_with(&format!("{third}/set"), &network::RequestContext::typed_navigation()).unwrap();

        // Clicking a link on a first-party page (initiator) to the third party.
        let handle = navigate_async(&format!("{third}/account"), network.clone(), Some(network::Initiator::for_url("http://127.0.0.1:1/page?x=SECRET")));
        poll_until_settled_navigation(&handle).unwrap().unwrap();

        let seen = seen.lock().unwrap().clone();
        let (_, cookie, referer) = seen.iter().find(|(p, _, _)| p == "/account").unwrap();
        assert_eq!(cookie.as_deref(), Some("lax_by_default=1"), "Lax rides a top-level cross-site navigation; Strict does not");
        assert_eq!(referer.as_deref(), Some("http://127.0.0.1:1/"), "origin only, no path or query");
    }

    #[test]
    fn local_storage_survives_navigation_within_an_origin_but_not_across_origins() {
        let base = spawn_server(|_| (200, vec![], b"<html><body><p>x</p></body></html>".to_vec()));
        let other = base.replace("127.0.0.1", "localhost");
        let network = network::NetworkContext::new();
        let mut cache = images::ImageCache::new();
        let first_url = format!("{base}/one");
        let (root, sheet) = fetch_page(&first_url, &mut cache, &network, None).unwrap();
        let page = Page::new(root, sheet, cache, Some(first_url), network.clone()).expect("test page has a body");
        page.js_env.borrow_mut().run_script("localStorage.setItem('theme', 'dark')").unwrap();

        // Navigate within the same origin: the new page (fresh JS environment) still has it.
        let (root2, sheet2, cache2, _) = poll_until_settled_navigation(&navigate_async(&format!("{base}/two"), network.clone(), None)).unwrap().unwrap();
        page.replace_with_navigation(root2, sheet2, cache2, Vec::new(), Some(format!("{base}/two"))).unwrap();
        assert_eq!(page.js_env.borrow_mut().run_script("localStorage.getItem('theme')").unwrap(), "dark");

        // Navigate to a different origin: it does not.
        let (root3, sheet3, cache3, _) = poll_until_settled_navigation(&navigate_async(&format!("{other}/three"), network.clone(), None)).unwrap().unwrap();
        page.replace_with_navigation(root3, sheet3, cache3, Vec::new(), Some(format!("{other}/three"))).unwrap();
        assert_eq!(page.js_env.borrow_mut().run_script("localStorage.getItem('theme')").unwrap(), "null");
    }

    // ---- Milestone 28: the built-in page has an opaque origin, not a free pass ----

    #[test]
    fn the_demo_page_cannot_read_a_cross_origin_response_without_cors() {
        let base = spawn_server(|path| match path {
            "/public" => (200, vec![("Access-Control-Allow-Origin".into(), "*".into())], b"public".to_vec()),
            _ => (200, vec![], b"private".to_vec()),
        });
        let page = demo_page();
        assert!(page.js_env.borrow().origin().is_opaque(), "a page with no URL has an opaque origin");
        page.js_env
            .borrow_mut()
            .run_script(&format!(
                "globalThis.a = globalThis.b = 'pending'; \
                 fetch('{base}/private').then(r => r.text()).then(t => {{ globalThis.a = t; }}).catch(() => {{ globalThis.a = 'blocked'; }}); \
                 fetch('{base}/public').then(r => r.text()).then(t => {{ globalThis.b = t; }}).catch(() => {{ globalThis.b = 'blocked'; }});"
            ))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while page.js_env.borrow().has_pending_fetches() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            page.js_env.borrow_mut().poll_pending_fetches();
        }
        assert_eq!(page.js_env.borrow_mut().run_script("globalThis.a").unwrap(), "blocked");
        assert_eq!(page.js_env.borrow_mut().run_script("globalThis.b").unwrap(), "public");
    }

    #[test]
    fn a_link_click_on_a_page_with_no_url_is_a_cross_site_navigation() {
        // It used to be sent as a typed (browser-initiated) navigation,
        // carrying even `SameSite=Strict` cookies.
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let third = third_party_server(Arc::clone(&seen));
        let network = network::NetworkContext::new();
        network.fetch_with(&format!("{third}/set"), &network::RequestContext::typed_navigation()).unwrap();

        let page = demo_page();
        let initiator = page.js_env.borrow().page().clone();
        let handle = navigate_async(&format!("{third}/account"), network.clone(), Some(initiator));
        poll_until_settled_navigation(&handle).unwrap().unwrap();

        let seen = seen.lock().unwrap().clone();
        let (_, cookie, referer) = seen.iter().find(|(p, _, _)| p == "/account").unwrap();
        assert_eq!(cookie.as_deref(), Some("lax_by_default=1"), "Lax rides a cross-site navigation; Strict does not");
        assert_eq!(*referer, None, "a page with no URL has nothing to send as Referer");
    }

    // ---- Hardening pass: startup failures are errors, and frames are reproducible ----

    #[test]
    fn a_document_with_no_body_is_an_error_not_a_panic() {
        // Built by hand: since M32 the parser implies a <body> for any input.
        let mut root = dom::Element::new("html");
        root.append_child(dom::Element::new("head"));
        let result = Page::new(root, css_parser::Stylesheet::default(), images::ImageCache::new(), None, network::NetworkContext::new());
        assert!(result.is_err_and(|e| e.contains("no <body>")));
    }

    /// The demo page's first frame, at 800x700 with a fixed clock, as a
    /// deterministic JSON snapshot. Recorded before Milestone 29 moved the
    /// pipeline into `engine` (commit 5d7fe76) and matched unchanged after
    /// it. Re-blessed once, in Milestone 31, for viewport culling: the new
    /// snapshot was checked to equal the old one with exactly its off-screen
    /// items removed (312 → 205 items; everything else byte-identical).
    /// Re-blessed in Milestone 33, which gave the page what a browser gives
    /// it: the document rooted at `<html>`, the user-agent sheet (`body`'s
    /// 8px margin, `p`'s 1em margins, collapsed with siblings'), the scene's
    /// canvas colour — checked by eye against the rendered PNG. And again
    /// for `display` (same milestone): form controls are inline-blocks, so
    /// the search box and its button share a line, as in a browser.
    /// `BLESS=1 cargo test -p engine demo_pages_first_frame` rewrites it.
    #[test]
    fn the_demo_pages_first_frame_matches_the_pre_move_golden_snapshot() {
        let page = demo_page();
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let scene = frame::build_frame(&page, &font, &mut atlas, 800.0, 700.0, Instant::now()).0;
        let json = rendering::scene_to_json(&scene);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/demo_page.scene.json");
        if std::env::var_os("BLESS").is_some() {
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
            std::fs::write(path, &json).unwrap();
        }
        let golden = std::fs::read_to_string(path).expect("golden snapshot missing: run with BLESS=1 once");
        assert!(json == golden, "the demo page's first frame changed; diff {path} against:\n{json}");
    }

    #[test]
    fn a_frame_built_with_a_fixed_clock_is_reproducible() {
        // The frame clock is injected, so identical input + identical `now`
        // gives an identical frame — the basis for deterministic replay.
        let now = Instant::now();
        let font = text::Font::dejavu_sans();
        let build = || {
            let page = demo_page();
            let mut atlas = text::GlyphAtlas::new(512, 128);
            frame::build_frame(&page, &font, &mut atlas, 800.0, 700.0, now).0
        };
        // Everything but element identities, which are node ids — unique
        // per process, so two documents built in one never share them.
        let anonymous = |scene: rendering::Scene| {
            let mut items = scene.items.as_ref().clone();
            for item in &mut items {
                if let rendering::DisplayItem::PushTransform(layer) = item {
                    layer.element = None;
                }
            }
            format!("{items:?}")
        };
        assert_eq!(anonymous(build()), anonymous(build()));
    }
}
