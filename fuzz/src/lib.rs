//! Fuzz-target bodies, shared between the libFuzzer binaries in
//! `fuzz_targets/` (nightly + `cargo fuzz`, real coverage-guided
//! fuzzing — run in CI by `scripts/fuzz-smoke.sh`) and the stable-Rust
//! replay/mutation tests in `tests/` (run anywhere `cargo test` runs,
//! no C++ toolchain or nightly needed).
//!
//! Every function takes arbitrary bytes and must NEVER panic, hang, or
//! overflow the stack. Returning normally is the only success
//! condition — the functions deliberately return `()`.
//!
//! Adding a target: (1) add a function here, (2) list it in `TARGETS`,
//! (3) add a one-line `fuzz_targets/<name>.rs` wrapper and a `[[bin]]`
//! entry, (4) commit seed inputs under `corpus/<name>/` (regenerate
//! with `python3 generate_seeds.py`). `tests/replay_corpus.rs` fails if
//! any of those four are missing.

use std::sync::OnceLock;

/// `(name, function)` for every fuzz target — the single list the
/// replay tests iterate, so a target cannot exist without being
/// exercised on stable.
pub const TARGETS: &[(&str, fn(&[u8]))] = &[
    ("parse_document", parse_document),
    ("parse_stylesheet", parse_stylesheet),
    ("parse_inline_style", parse_inline_style),
    ("decode_auto", decode_auto),
    ("url_origin", url_origin),
    ("cookie_header", cookie_header),
    ("layout_document", layout_document),
    ("pattern_matches", pattern_matches),
    ("psl_parse", psl_parse),
    ("origin_parse", origin_parse),
    ("font_parse", font_parse),
    ("font_face_src", font_face_src),
    ("content_decoding", content_decoding),
    ("http_headers", http_headers),
    ("js_script", js_script),
    ("webidl_parse", webidl_parse),
    ("css_length", css_length),
    ("css_color", css_color),
    ("css_background", css_background),
    ("css_grid", css_grid),
    ("ipc_decode", ipc_decode),
    ("web_font", web_font),
    ("profile_files", profile_files),
    ("filter_list", filter_list),
    ("dns_message", dns_message),
];

fn utf8(data: &[u8]) -> Option<&str> {
    // Only valid UTF-8: every one of these parsers takes `&str`; the
    // interesting failures are in malformed *markup/values*, not
    // malformed *encoding* (`FetchResponse::body_as_string` is lossy).
    std::str::from_utf8(data).ok()
}

/// `html_parser::parse_document` — the tokenizer + tree builder — and,
/// since Milestone 40b, the move into a `dom::Document`, which interns the
/// names the page invented in its own table. Every name must read back as
/// it was parsed.
pub fn parse_document(data: &[u8]) {
    if let Some(s) = utf8(data) {
        let Ok(root) = html_parser::parse_document(s) else { return };
        let names: Vec<(String, Vec<String>)> = dom::ElementRef::from(&root).descendants_and_self().map(|e| (e.tag().to_string(), e.attributes().map(|(k, _)| k.to_string()).collect())).collect();
        let document = dom::Document::new(root);
        let interned = document.root().descendants_and_self().map(|e| (e.tag().to_string(), e.attributes().map(|(k, _)| k.to_string()).collect()));
        assert!(interned.eq(names), "a name changed on its way into the document");
    }
}

/// `css_parser::parse_stylesheet` — CSS from `<link>`/`<style>`: deep
/// `:not()` nesting, unterminated blocks, huge selector lists.
pub fn parse_stylesheet(data: &[u8]) {
    if let Some(s) = utf8(data) {
        let _ = css_parser::parse_stylesheet(s);
    }
}

/// `css_parser::parse_inline_style` — `style="..."` attributes (and
/// every JS `element.style.x = ...` write).
pub fn parse_inline_style(data: &[u8]) {
    if let Some(s) = utf8(data) {
        let _ = css_parser::parse_inline_style(s);
    }
}

/// `images::decode_auto` — format sniffing plus the PNG/JPEG/GIF/WebP
/// decoders, fed straight from the network. Header bombs, truncated
/// streams, mismatched magic numbers.
pub fn decode_auto(data: &[u8]) {
    let _ = images::decode_auto(data);
}

/// `network::resolve_url` plus origin/site computation on the result —
/// every `href`/`src`/`url(...)`, and the same-origin policy's foundation.
/// Input is `base\nrelative`; the halves are also tried as bare URLs.
pub fn url_origin(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let (base, relative) = s.split_once('\n').unwrap_or((s, ""));
    if let Some(resolved) = network::resolve_url(base, relative) {
        let _ = origin::Origin::parse(&resolved).site();
    }
    let a = origin::Origin::parse(base);
    let b = origin::Origin::parse(relative);
    // Origin and site comparison must be total.
    let _ = (a == b, a.same_site(&b));

    // Site (registrable-domain) computation, on hostile hosts.
    let _ = origin::psl::registrable_domain_or_host(relative);
    let _ = origin::psl::is_public_suffix(base);

    // Referer computation, on hostile initiator/target pairs and both policies.
    if let Ok(target) = url::Url::parse(relative) {
        for policy in [network::ReferrerPolicy::NoReferrer, network::ReferrerPolicy::StrictOriginWhenCrossOrigin] {
            let _ = network::referer_for(policy, base, &target);
        }
    }
    let _ = network::loggable_url(base);
}

/// Cookie parsing/matching/persistence: `store_from_header` (a value
/// chosen by a remote server), `header_for` (attacker-chosen
/// domain/path), `load_from_str` (an on-disk file that could be
/// corrupted or tampered with). Input is `header\ndomain\npath`.
pub fn cookie_header(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let mut parts = s.splitn(3, '\n');
    let header = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("example.com");
    let path = parts.next().unwrap_or("/");

    let mut jar = network::cookies::CookieJar::new();
    jar.store_from_header(header, domain);
    // The secure-origin-aware store, both ways.
    jar.store(header, domain, false);
    jar.store(header, domain, true);
    let _ = jar.header_for(domain, path, true);
    let _ = jar.header_for(domain, path, false);
    // Cross-site / navigation contexts (SameSite), with hostile hosts.
    let site = origin::Origin::parse(&format!("https://{domain}/")).site();
    let evil = origin::Origin::parse("https://evil.test/").site();
    let opaque = origin::Origin::new_opaque().site();
    for (initiator, is_navigation) in [(None, false), (Some(&evil), false), (Some(&evil), true), (Some(&site), false), (Some(&opaque), false)] {
        let _ = jar.header_for_request(&network::cookies::SendContext {
            host: domain,
            path,
            is_secure: true,
            request_site: &site,
            initiator_site: initiator,
            is_navigation,
        });
    }

    // Whatever the jar serializes it must load back without panicking.
    let serialized = jar.serialize();
    let mut reloaded = network::cookies::CookieJar::new();
    reloaded.load_from_str(&serialized);
    // And the raw input is itself a hostile persisted-jar file.
    reloaded.load_from_str(s);
    jar.purge_expired(0);
}

fn font() -> &'static text::Font {
    static FONT: OnceLock<text::Font> = OnceLock::new();
    FONT.get_or_init(text::Font::dejavu_sans)
}

/// End to end: HTML + CSS → parse → cascade (parallel) → layout →
/// display list. Catches parser→style→layout→paint panics a per-crate
/// target cannot, because each later stage only ever sees the earlier
/// stage's *valid-looking* output. Input is `html\0css`; with no NUL
/// the whole input is HTML.
pub fn layout_document(data: &[u8]) {
    with_layout_stack(data, layout_document_inner);
}

/// Runs `f` on a thread with the stack layout documents that it needs
/// (`layout::MIN_STACK_BYTES`, what the engine's page threads provide), so
/// deep-nesting inputs test the engine's conditions rather than a test
/// thread's smaller default. A panic in `f` is re-raised here.
fn with_layout_stack(data: &[u8], f: fn(&[u8])) {
    let data = data.to_vec();
    let result = std::thread::Builder::new().stack_size(layout::MIN_STACK_BYTES).spawn(move || f(&data)).expect("spawn").join();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn layout_document_inner(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let (html, css) = s.split_once('\0').unwrap_or((s, ""));

    let Ok(root) = html_parser::parse_document(html) else { return };
    let stylesheet = css_parser::parse_stylesheet(css);

    let mut image_cache = images::ImageCache::new();
    let laid_out = layout::layout_document(&root, 800.0, font(), &stylesheet, &mut image_cache);

    let mut atlas = text::GlyphAtlas::new(512, 512);
    let _ = rendering::display_list::build_display_list(&laid_out, font(), &mut atlas);

    // Milestone 39b: hit testing pruned by the recorded subtree bounds finds
    // what visiting every box finds, wherever the input puts its boxes; and
    // a page's banded list builds.
    let mut reference = laid_out.clone();
    let mut stack = vec![&mut reference];
    while let Some(b) = stack.pop() {
        b.subtree.bounds = None;
        b.subtree.ordered = false;
        stack.extend(b.children.iter_mut());
    }
    let scrolls = layout::ScrollOffsets::new();
    for (i, &byte) in data.iter().take(8).enumerate() {
        let (x, y) = (f32::from(byte) * 3.3, (i as f32) * 97.0 + f32::from(byte));
        let pruned = layout::hit_test_counted(&laid_out, x, y, 0.0, &scrolls);
        assert_eq!(pruned.path, layout::hit_test_path(&reference, x, y), "pruned hit testing at ({x}, {y})");
        assert_eq!(pruned.path.len(), pruned.boxes.len());
    }
    let _ = rendering::PageDisplayList::build(&laid_out, font(), &mut atlas, &scrolls);
}

/// `pattern::matches` — the `pattern` attribute of a form control, the one
/// place page-supplied regular expressions are evaluated. Input is
/// `pattern\nvalue`. Matching must be linear in the value whatever the
/// pattern (no catastrophic backtracking), compilation bounded, and a
/// pattern that needs backtracking rejected — never a panic or a hang.
pub fn pattern_matches(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let (pattern, value) = s.split_once('\n').unwrap_or((s, ""));
    let _ = pattern::matches(pattern, value);
    // And the whole input as a value against a few fixed hostile patterns.
    for fixed in ["(a+)+$", "(a|aa)+$", "^(([a-z])+.)+[A-Z]([a-z])+$", r"\d{3}-\d{4}"] {
        let _ = pattern::matches(fixed, s);
    }
}

/// `origin::psl::PublicSuffixList::parse` and the site queries on it. The
/// vendored list is trusted data, but the update script parses a downloaded
/// one and the parser must never be the thing that fails on a malformed
/// list. Input is `list text \0 host` (host queried against the parsed list).
pub fn psl_parse(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let (list_text, host) = s.split_once('\0').unwrap_or((s, "a.b.c"));
    let list = origin::psl::PublicSuffixList::parse(list_text);
    let _ = list.rule_count();
    let _ = list.registrable_domain(host);
    let _ = list.is_public_suffix(host);
    // Every line of the input is also a host to look up.
    for line in list_text.lines().take(64) {
        let _ = list.registrable_domain(line);
    }
}

/// `origin::Origin::parse` and everything derived from it — the input of
/// every same-origin, same-site, storage-partition and CORS decision
/// (Milestone 28, ADR 004). Input is `url \n other-url \n allow-origin`.
/// Every operation must be total: no panic, bounded time, and the
/// equivalence properties the policies rely on must hold.
pub fn origin_parse(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let mut parts = s.splitn(3, '\n');
    let a_text = parts.next().unwrap_or("");
    let b_text = parts.next().unwrap_or("");
    let allow = parts.next();

    let a = origin::Origin::parse(a_text);
    let b = origin::Origin::parse(b_text);
    let (sa, sb) = (a.site(), b.site());
    let _ = (a.serialize(), b.serialize(), sa.partition_key(), sb.partition_key());
    let _ = (a.host(), a.port(), a.scheme(), b.to_string(), sa.to_string());

    // Same-origin implies same-site; an origin is always same-origin with
    // itself (opaque included); re-parsing a tuple origin's serialisation
    // gives the same origin back.
    assert!(!a.same_origin(&b) || a.same_site(&b), "same-origin but not same-site: {a_text:?} / {b_text:?}");
    assert!(a.same_origin(&a.clone()));
    if !a.is_opaque() {
        assert_eq!(origin::Origin::parse(&a.serialize()), a, "serialisation must round-trip: {a_text:?}");
    }

    let key = origin::StorageKey::embedded(&a, &b);
    let _ = key.is_shareable();
    for credentials in [false, true] {
        let _ = origin::cors_response_readable(&a, &b, credentials, allow, Some("true"));
        let _ = origin::cors_response_readable(&a, &b, credentials, Some(b_text), allow);
    }
}

// ---- Milestone 32: the targets required before web fonts (M35) ----

/// `text::Font::from_bytes` and glyph work on the result — a TTF/OTF file.
/// Web fonts (M35) will make font parsing reachable from the network, so it
/// is fuzzed first: malformed tables, huge glyph counts, recursive composite
/// glyphs. Sizes are the engine's own bounds (`style::MAX_FONT_SIZE_PX`).
pub fn font_parse(data: &[u8]) {
    let Ok(font) = text::Font::from_bytes(data) else { return };
    let mut atlas = text::GlyphAtlas::new(256, 64);
    for size in [1.0, 16.0, 97.5] {
        let _ = font.measure("Hg, 世界 ب", size);
        for ch in ['H', 'g', '世', '\u{0}', '\u{10FFFF}'] {
            let _ = font.rasterize(ch, size);
            let _ = atlas.get_or_insert(&font, ch, size);
        }
    }
}

/// Milestone 35: a fetched web font, as the engine takes it — sniffed and
/// decoded (TTF/OTF, WOFF, WOFF2), added to a font set, then shaped (every
/// script's shaper, right to left too) and drawn through the atlas.
pub fn web_font(data: &[u8]) {
    let Ok(bytes) = text::decode_web_font(data) else { return };
    let font = text::Font::dejavu_sans();
    if font.add_web_font("fuzz", bytes, 400, text::FontStyle::Normal).is_err() {
        return;
    }
    let mut atlas = text::GlyphAtlas::new(256, 64);
    let sel = font.selector(&["fuzz"], 700, text::FontStyle::Italic);
    for (text, rtl) in [("Hg fi AV", false), ("سلام", true), ("שלום", true), ("नमस्ते", false), ("世界", false)] {
        let run = font.shape(text, &text::TextSpec { selector: sel, size: 24.0, rtl });
        for g in &run.glyphs {
            let _ = atlas.get_or_insert_glyph(&font, g.glyph, 24.0);
        }
    }
}

/// `css_parser::parse_font_face_src` and whole `@font-face` rules — the
/// input that decides which font bytes the engine fetches once web fonts
/// exist.
pub fn font_face_src(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let _ = css_parser::parse_font_face_src(s);
    let _ = css_parser::parse_stylesheet(&format!("@font-face {{ font-family: x; src: {s} }}"));
}

/// Milestone 33: `style::parse_length` (units, `calc()`, `min()`/`max()`/
/// `clamp()`) directly, then through the cascade as every length-taking
/// property. A resolved size or font size must be finite, and `font-size`
/// bounded — layout and glyph rasterization allocate from them.
pub fn css_length(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    for ctx in [
        style::LengthContext::DEFAULT,
        style::LengthContext { font_size: 0.0, root_font_size: 1024.0, viewport_width: 1.0, viewport_height: 16384.0 },
    ] {
        if let Some(l) = style::parse_length(s, &ctx) {
            assert!(l.px.is_finite() && l.percent.is_finite(), "non-finite length from {s:?}");
        }
    }
    if s.contains([';', '{', '}']) {
        return;
    }
    let css = format!(
        "font-size: {s}; width: {s}; height: {s}; margin: {s}; padding: {s} {s}; border: {s} solid; flex-basis: {s}; gap: {s}; border-radius: {s}"
    );
    let mut el = dom::Element::new("div");
    el.set_attr("style", &css);
    let styled = style::resolve_style(&el, &[], None, &css_parser::Stylesheet::default());
    assert!(styled.font_size.is_finite() && styled.font_size <= style::MAX_FONT_SIZE_PX);
    for l in [styled.width, styled.height, styled.flex_basis].into_iter().flatten() {
        assert!(l.px.is_finite() && l.percent.is_finite());
    }
}

/// Milestone 33: `style::parse_color` — hex, `rgb()`/`hsl()` in both
/// syntaxes, named colours — directly and through every colour property.
pub fn css_color(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let _ = style::parse_color(s, style::Color::TRANSPARENT);
    if s.contains([';', '{', '}']) {
        return;
    }
    let mut el = dom::Element::new("div");
    el.set_attr("style", &format!("color: {s}; background-color: {s}; border: 1px solid {s}; outline-color: {s}; box-shadow: {s}; box-shadow: 1px 2px {s}"));
    let _ = style::resolve_style(&el, &[], None, &css_parser::Stylesheet::default());
}

/// Milestone 33: the `background` shorthand, `background-position` /
/// `-size` / `-repeat` parsers, through the cascade. (Placing the image
/// from what they produce — including degenerate and non-finite sizes — is
/// covered by `rendering`'s `image_placement` tests.)
pub fn css_background(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    if s.contains([';', '{', '}']) {
        return;
    }
    for property in ["background", "background-position", "background-size", "background-repeat"] {
        let mut el = dom::Element::new("div");
        el.set_attr("style", &format!("width: 50px; height: 40px; background-image: url(x.png); {property}: {s}"));
        let _ = style::resolve_style(&el, &[], None, &css_parser::Stylesheet::default());
    }
    // Milestone 33: the `content` value of generated content.
    let mut el = dom::Element::new("div");
    el.set_attr("title", s);
    let sheet = css_parser::parse_stylesheet(&format!("div::before {{ content: {s} }} div:after {{ content: attr(title) {s} }}"));
    let cache = style::resolve_style_tree_parallel(&el, &sheet, 800.0);
    for which in [style::PseudoElement::Before, style::PseudoElement::After] {
        if let Some(text) = cache.get(&style::pseudo_id(el.node_id, which)).and_then(|p| p.content.as_ref()) {
            assert!(text.chars().count() <= style::MAX_CONTENT_CHARS);
        }
    }
}

/// Milestone 33: CSS Grid — track lists (`repeat()`, `minmax()`, `fr`),
/// placements, template areas — parsed, then a small grid laid out and
/// painted with the value in every grid property.
pub fn css_grid(data: &[u8]) {
    with_layout_stack(data, css_grid_inner);
}

fn css_grid_inner(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    if s.contains([';', '{', '}', '<']) {
        return;
    }
    let css = format!(
        "#g {{ display: grid; width: 300px; grid-template-columns: {s}; grid-template-rows: {s}; grid-template-areas: {s}; grid-auto-rows: {s} }} \
         .i {{ grid-column: {s}; grid-row: {s} }} .j {{ grid-area: {s} }}"
    );
    let html = "<html><body><div id=g><p class=i>a</p><p class=j>b</p><p>c</p><p class=i>d</p></div></body></html>";
    let Ok(root) = html_parser::parse_document(html) else { return };
    let sheet = css_parser::parse_stylesheet(&css);
    let laid = layout::layout_document(&root, 800.0, font(), &sheet, &mut images::ImageCache::new());
    let mut atlas = text::GlyphAtlas::new(512, 512);
    let _ = rendering::display_list::build_display_list(&laid, font(), &mut atlas);
}

/// `network::encoding::decode_body` — a compressed body chosen by a server.
/// The first byte picks the `Content-Encoding`; the output is capped (a
/// compression bomb must hit the cap, not memory).
pub fn content_decoding(data: &[u8]) {
    let Some((&selector, body)) = data.split_first() else { return };
    let encoding = ["gzip", "x-gzip", "deflate", "identity", "gzip, gzip", "gzip, deflate", "br", ""][selector as usize % 8];
    let _ = network::encoding::decode_body(Some(encoding), body.to_vec(), 1 << 20);
}

/// Response header handling a server controls: `Cache-Control`, `ETag`,
/// `Last-Modified`, `Expires`, `Vary` through the HTTP cache, and
/// `Content-Type` parsing. Input is `Name: value` lines.
pub fn http_headers(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let headers: Vec<(String, String)> = s
        .lines()
        .take(256)
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    for (_, value) in &headers {
        let _ = network::parse_content_type(value);
    }
    let response = network::FetchResponse { status: 200, final_url: "https://example.test/x".into(), headers: headers.clone(), body: b"body".to_vec() };
    let mut cache = network::cache::HttpCache::default();
    let _ = cache.after_response("k", &response);
    let _ = cache.before_request("k");
    let not_modified = network::FetchResponse { status: 304, ..response };
    let _ = cache.after_response("k", &not_modified);
    let _ = cache.before_request("k");
}

/// A network route that refuses every request: script run by the fuzzer
/// must never reach the real network.
struct NoNetwork;
impl network::NetworkRoute for NoNetwork {
    fn fetch(&self, _url: &str) -> Result<network::FetchResponse, network::NetworkError> {
        Err(network::NetworkError::Transport("fuzzing: no network".into()))
    }
}

/// Arbitrary script against a fixed DOM through the JS→DOM bindings, then
/// the event paths (click, keys, rAF, fetch settlement). A script error is
/// fine — hostile pages throw — but a Rust panic is not.
pub fn js_script(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let mut body = dom::Element::new("body");
    for (tag, id) in [("div", "a"), ("input", "i"), ("form", "f"), ("a", "l"), ("p", "p")] {
        let mut el = dom::Element::new(tag);
        el.set_attr("id", id);
        if tag == "a" {
            el.set_attr("href", "/next");
        }
        body.append_child(el);
    }
    let document = std::rc::Rc::new(std::cell::RefCell::new(dom::Document::new(body)));
    let network = network::NetworkContext::with_route(|_store| std::sync::Arc::new(NoNetwork));
    let mut env = js::JsEnvironment::new_with_network(std::rc::Rc::clone(&document), Some("https://fuzz.test/"), network);
    let _ = env.run_script(s);
    let ids: Vec<dom::NodeId> = ["a", "i", "l"].iter().filter_map(|id| document.borrow().node_id_of(id)).collect();
    env.dispatch_click(&ids);
    env.set_focus(Some("i".into()));
    for key in ["x", "Backspace", "Enter", "ArrowLeft"] {
        env.dispatch_keydown(key);
    }
    env.run_animation_frame_callbacks(16.0);
    env.poll_pending_fetches();
    let _ = env.take_pending_navigation();
}

/// Milestone 40 (ADR 023): the WebIDL subset the bindings are declared in.
/// Its input is the engine's own files, read at build time, but it is a
/// parser like any other here: bounded, and never panicking.
pub fn webidl_parse(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let _ = webidl::parse(s);
}

/// ADR 016: the messages a renderer process sends the host, and the host
/// sends a renderer — the process-isolation trust boundary. The host decodes
/// what a possibly compromised renderer wrote, so the decoder must bound
/// every length and never panic, whatever the bytes.
pub fn ipc_decode(data: &[u8]) {
    engine::fuzz_decode_ipc_message(data);
}

/// Milestone 36 (ADR 010): the files Browser Core reads back from a profile
/// directory — settings, permission decisions, local storage. Whatever is on
/// disk (damaged, truncated, written by something else), reading never
/// panics, never yields a setting outside its range, and what it yields
/// writes back and reads again as itself. The input is tried as it is and
/// behind each file's header, so mutations reach the line parsers.
pub fn profile_files(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    for text in [s.to_string(), format!("{}\n{s}", browser_core::SETTINGS_HEADER)] {
        let (settings, _) = browser_core::Settings::parse(&text);
        assert!(browser_core::SCROLL_SPEED_RANGE.contains(&settings.scroll_speed()));
        assert!(browser_core::ZOOM_RANGE.contains(&settings.default_zoom()));
        assert_eq!(browser_core::Settings::parse(&settings.serialize()).0, settings);
    }
    for text in [s.to_string(), format!("{}\n{s}", browser_core::PERMISSIONS_HEADER)] {
        let (permissions, _) = browser_core::PermissionStore::parse(&text);
        assert!(permissions.decisions().len() <= browser_core::MAX_DECISIONS);
        assert_eq!(browser_core::PermissionStore::parse(&permissions.serialize()).0.decisions(), permissions.decisions());
    }
    for text in [s.to_string(), format!("{}\n{s}", storage::FILE_HEADER)] {
        let store = storage::StorageStore::new();
        store.load_from_str(&text);
        assert!(store.origin_count() <= storage::MAX_STORAGE_ORIGINS);
        let again = storage::StorageStore::new();
        again.load_from_str(&store.serialize());
        assert_eq!(again.serialize(), store.serialize());
    }
}

/// Milestone 37 (ADR 019 §1): a filter list is untrusted input — parsing
/// stays within its bounds, and matching the rules it produced against URLs
/// (some built from the input itself) never panics or hangs.
pub fn filter_list(data: &[u8]) {
    let Some(s) = utf8(data) else { return };
    let mut engine = filter::FilterEngine::new();
    if engine.add_list(filter::Category::Ads, s).is_err() {
        return;
    }
    assert!(engine.len() <= filter::MAX_RULES);
    let tail: String = s.chars().filter(|c| c.is_ascii_graphic()).take(200).collect();
    for url in ["https://ads.example/banner/1.png", "http://127.0.0.1:8080/a?b=c#d", "https://x.example/", &format!("https://h.example/{tail}")] {
        for kind in [filter::RequestType::Script, filter::RequestType::Document, filter::RequestType::Image] {
            for third_party in [false, true] {
                let _ = engine.check(&filter::FilterRequest { url, kind, page_host: Some("news.example"), third_party });
            }
        }
    }
}

/// Milestone 37 (ADR 012): a DoH response comes from a resolver the user
/// chose, over the network — decoding is bounded and never panics,
/// compression loops included.
pub fn dns_message(data: &[u8]) {
    for qtype in [network::dns::QueryType::A, network::dns::QueryType::Aaaa] {
        if let Ok(answer) = network::dns::decode_answers(data, "example.com", qtype) {
            assert!(answer.addresses.len() <= network::dns::MAX_ANSWERS);
        }
    }
    if let Some(name) = utf8(data) {
        let _ = network::dns::encode_query(name, network::dns::QueryType::A);
    }
}
