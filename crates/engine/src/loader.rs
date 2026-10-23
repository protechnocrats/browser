//! Milestone 8 page-loading logic, factored out of `main.rs` so it can
//! be tested end-to-end against a real (local) HTTP server rather than
//! only exercised by hand.
//!
//! `fetch_page` fetches a real document over the network and runs it
//! through the front half of the pipeline — parse → collect linked
//! resources → fetch them (concurrently) → produce a DOM + a combined
//! stylesheet, with every referenced image decoded into the shared
//! `images::ImageCache` under the same absolute-URL key `layout` will
//! later look it up by.
//!
//! What this covers that the pre-existing `main.rs` copy did not
//! (both were in Milestone 8's stated scope — "fetching
//! `<link rel="stylesheet">` CSS and `<img src="...">` /
//! `background-image: url(...)` images referenced by it"):
//!
//! - **Relative `<img src>` values.** The old code only collected
//!   `http(s)://`-absolute `src`s; a real page's `<img src="/logo.png">`
//!   was silently skipped. Every `src` is now resolved against the
//!   document's (post-redirect) final URL via `network::resolve_url`,
//!   the DOM attribute is rewritten to the absolute form, and the image
//!   is fetched and cached under that absolute key.
//! - **`background-image: url(...)`.** The old code never fetched CSS
//!   background images over the network at all. Every `url(...)` in the
//!   combined stylesheet text is now resolved to an absolute URL,
//!   rewritten in place (so `style`'s `background_image_path` ends up
//!   absolute, matching the cache key), and fetched.
//!
//! Simplification kept from Milestone 8's scope: `url(...)` references
//! inside an external stylesheet are resolved against the *page* URL,
//! not the stylesheet's own URL. Correct for the common case (same
//! origin, sheet at the site root); a sheet loaded from a CDN with its
//! own relative asset paths would resolve wrong. Noted here rather than
//! silently — real per-sheet base URLs are Phase 2 work.

use std::time::{Duration, Instant};

/// Fetches `url`, its linked stylesheets, and every image it references
/// (`<img>` and `background-image`), returning the parsed DOM and the
/// combined stylesheet. Decoded image bytes are inserted into
/// `image_cache` keyed by absolute URL — the same key `layout`'s
/// `load_from_file`/`preload_parallel` calls will hit, so no filesystem
/// access happens for a network image.
///
/// Milestone 26: every request goes through the caller's shared
/// `network` context — one cookie jar and HTTP cache per profile — so a
/// cookie the page load receives is visible to the page's own later
/// `fetch()`, and to the next navigation.
///
/// `initiator` is the document that triggered this load (a link click or
/// form submit) — its origin, possibly opaque, and URL — or `None` for a
/// browser-initiated load (a typed URL, the startup page). It decides which cookies ride on the document
/// request (`SameSite`) and what `Referer` it carries; the page's own
/// stylesheets and images are then subresource requests OF the loaded
/// document, so third-party ones are cross-site (no `Strict`/`Lax`
/// cookies) and the HTTP cache is partitioned by this page's site.
#[cfg(test)]
pub fn fetch_page(
    url: &str,
    image_cache: &mut images::ImageCache,
    network: &network::NetworkContext,
    initiator: Option<&network::Initiator>,
) -> Result<(dom::Element, css_parser::Stylesheet), String> {
    let (root, stylesheet, pending, _) = fetch_page_progressive(url, image_cache, network, initiator)?;
    wait_for_images(pending, image_cache);
    Ok((root, stylesheet))
}

/// How a page's images reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageLoading {
    /// The page is shown once its document and stylesheets have loaded; each
    /// image appears when it arrives (performance audit: the first frame
    /// used to wait for every image on the page).
    #[default]
    Progressive,
    /// The first frame waits for every image — deterministic, for hosts that
    /// compare frames (the conformance corpus).
    BeforeFirstFrame,
}

/// An image a page is still waiting for.
pub struct PendingImage {
    /// The absolute URL — the image cache's key.
    pub url: String,
    state: ImageState,
}

enum ImageState {
    Fetching(network::FetchHandle),
    /// Milestone 34: arrived, and decoding on the worker pool — off the
    /// page thread, which only collects the result.
    Decoding(std::sync::mpsc::Receiver<Option<images::DecodedImage>>),
}

/// How long a page load waits for its subresources, all together (not per
/// resource: ten slow ones used to wait ten times as long).
pub const SUBRESOURCE_DEADLINE: Duration = Duration::from_secs(10);

/// [`fetch_page`], up to the images: returns once the document and its
/// stylesheets (which block rendering) have loaded, with the page's image
/// requests in flight, and the URL the document came from — the one it
/// landed on after any redirects, which is its address and names its
/// origin. Images already in `image_cache` are not requested.
pub fn fetch_page_progressive(
    url: &str,
    image_cache: &mut images::ImageCache,
    network: &network::NetworkContext,
    initiator: Option<&network::Initiator>,
) -> Result<(dom::Element, css_parser::Stylesheet, Vec<PendingImage>, String), String> {
    use network::RequestContext;

    let document_context = match initiator {
        Some(page) => RequestContext::navigation_by(page.clone()),
        None => RequestContext::typed_navigation(),
    };
    let response = network.fetch_with(url, &document_context).map_err(|e| e.to_string())?;
    // Resolve every relative reference against the URL we actually
    // landed on (after any redirects), not the one originally asked for.
    let base_url = response.final_url.clone();
    // The body is decoded as UTF-8 (lossily): count other encodings for the
    // conformance report (a no-op unless enabled).
    if let Some(charset) = response.header("content-type").and_then(|v| network::parse_content_type(v).charset) {
        if charset != "utf-8" && charset != "utf8" && charset != "us-ascii" {
            diagnostics::unsupported::record("charset", &charset);
        }
    }
    // Milestone 37: what each request is for — what the filter's type
    // options test (ADR 019).
    let sheet_context = RequestContext::subresource_of(&base_url).with_destination(network::Destination::Stylesheet);
    let image_context = RequestContext::subresource_of(&base_url).with_destination(network::Destination::Image);
    let html = response.body_as_string();
    let mut root = {
        let _s = diagnostics::tracing::info_span!("parse_html", bytes = html.len() as u64).entered();
        html_parser::parse_document(&html).map_err(|_| "fetched document has no <html> element".to_string())?
    };

    let mut sheet_links = Vec::new();
    collect_stylesheet_links(dom::ElementRef::from(&root), &mut sheet_links);

    // Rewrite every <img src> to an absolute URL in place and collect
    // the absolute URLs to fetch.
    let mut image_urls = Vec::new();
    rewrite_img_srcs_to_absolute(&mut root, &base_url, &mut image_urls);

    // Dispatch every stylesheet fetch AT ONCE (real background threads),
    // then collect the results — the round-trips overlap rather than
    // serializing.
    let sheet_handles: Vec<network::FetchHandle> = sheet_links
        .iter()
        .filter_map(|href| network::resolve_url(&base_url, href))
        .map(|resolved| network.fetch_async_with(&resolved, &sheet_context))
        .collect();

    let mut style_text = String::new();
    collect_style_text(dom::ElementRef::from(&root), &mut style_text);
    let deadline = Instant::now() + SUBRESOURCE_DEADLINE;
    for handle in &sheet_handles {
        if let Some(Ok(sheet_response)) = handle.wait_until(deadline) {
            style_text.push('\n');
            // Milestone 35: a stylesheet's `url()`s are relative to the
            // stylesheet, not the document (`@font-face` fonts in
            // `../fonts/` resolved against the page and were never found).
            let sheet_url = sheet_response.final_url.clone();
            style_text.push_str(&rewrite_css_image_urls(&sheet_response.body_as_string(), &sheet_url, &mut image_urls));
        }
    }

    // Now that the full CSS text is assembled, rewrite its `url(...)`
    // references to absolute URLs (so `style` records an absolute
    // `background_image_path`) and collect them alongside the `<img>`
    // URLs.
    let style_text = rewrite_css_image_urls(&style_text, &base_url, &mut image_urls);
    let stylesheet = {
        let _s = diagnostics::tracing::info_span!("parse_css", bytes = style_text.len() as u64).entered();
        css_parser::parse_stylesheet(&style_text)
    };

    // De-duplicate, skip anything already cached, and request the whole
    // image set concurrently; the caller waits for it or takes it as it
    // arrives.
    image_urls.sort();
    image_urls.dedup();
    image_urls.retain(|u| !image_cache.contains(u));
    let pending: Vec<PendingImage> = image_urls
        .into_iter()
        .map(|url| {
            image_cache.mark_pending(&url);
            let handle = network.fetch_async_with(&url, &image_context);
            PendingImage { url, state: ImageState::Fetching(handle) }
        })
        .collect();

    Ok((root, stylesheet, pending, base_url))
}

/// Waits (until [`SUBRESOURCE_DEADLINE`], for all of them together) for
/// `pending` and decodes them into `cache`.
pub fn wait_for_images(pending: Vec<PendingImage>, cache: &mut images::ImageCache) {
    wait_for_images_until(pending, cache, Instant::now() + SUBRESOURCE_DEADLINE);
}

/// [`wait_for_images`], until `deadline`.
pub fn wait_for_images_until(pending: Vec<PendingImage>, cache: &mut images::ImageCache, deadline: Instant) {
    let mut arrived: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    for p in pending {
        match p.state {
            ImageState::Fetching(handle) => {
                let body = match handle.wait_until(deadline) {
                    Some(Ok(response)) => Some(response.body),
                    _ => None,
                };
                arrived.push((p.url, body));
            }
            ImageState::Decoding(decoded) => {
                let timeout = deadline.saturating_duration_since(Instant::now());
                file_decoded(cache, &p.url, decoded.recv_timeout(timeout).ok().flatten());
            }
        }
    }
    decode_into(arrived, cache);
}

/// Files the images of `pending` whose decoding has finished in `cache`
/// and returns their URLs — every one that finished, decoded or not. One
/// that has just arrived starts decoding on the worker pool (Milestone 34:
/// never on the page thread, which used to wait for a tick's decodes); it
/// is collected by a later call.
pub fn take_arrived_images(pending: &mut Vec<PendingImage>, cache: &mut images::ImageCache, notify: Option<&network::Notify>) -> Vec<String> {
    let mut finished = Vec::new();
    pending.retain_mut(|p| {
        if let ImageState::Fetching(handle) = &p.state {
            let Some(result) = handle.try_recv() else { return true };
            p.state = ImageState::Decoding(decode_in_background(result.ok().map(|r| r.body), notify.cloned()));
        }
        let ImageState::Decoding(decoded) = &p.state else { return true };
        let image = match decoded.try_recv() {
            Ok(image) => image,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        file_decoded(cache, &p.url, image);
        finished.push(p.url.clone());
        false
    });
    finished
}

/// Decodes `body` on the worker pool; the result arrives on the receiver. A
/// decoder that panics on hostile bytes yields "does not decode" — a panic
/// on a pool thread would otherwise abort the process. `notify` wakes the
/// page when the result is ready (ADR 020).
fn decode_in_background(body: Option<Vec<u8>>, notify: Option<network::Notify>) -> std::sync::mpsc::Receiver<Option<images::DecodedImage>> {
    let (tx, rx) = std::sync::mpsc::channel();
    match body {
        None => {
            let _ = tx.send(None);
        }
        Some(bytes) => rayon::spawn(move || {
            let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| images::decode_auto(&bytes).ok())).ok().flatten();
            let _ = tx.send(image);
            if let Some(notify) = notify {
                notify();
            }
        }),
    }
    rx
}

fn file_decoded(cache: &mut images::ImageCache, url: &str, image: Option<images::DecodedImage>) {
    match image {
        Some(image) => {
            cache.insert_decoded(url, image);
        }
        None => cache.finish_pending(url),
    }
}

/// Decodes the bodies in parallel (a page's images used to be decoded one
/// after another on the loading thread), then files them in `cache`; a URL
/// with no body, or one that does not decode, is no longer pending.
fn decode_into(arrived: Vec<(String, Option<Vec<u8>>)>, cache: &mut images::ImageCache) {
    use rayon::prelude::*;
    let decoded: Vec<(String, Option<images::DecodedImage>)> =
        arrived.into_par_iter().map(|(url, body)| {
            let image = body.and_then(|b| images::decode_auto(&b).ok());
            (url, image)
        }).collect();
    for (url, image) in decoded {
        file_decoded(cache, &url, image);
    }
}

/// Milestone 21: the result `navigate_async`'s background thread sends
/// back — everything `main.rs` needs to swap the whole pipeline over
/// to the new page in one go (a fresh, page-scoped `ImageCache`, not
/// the caller's old one — the old page's images are irrelevant to the
/// new page and keeping them around would only leak memory across
/// navigations).
/// The new document, its stylesheet and image cache, (with
/// [`ImageLoading::Progressive`]) the image requests still in flight, and
/// its URL — where it landed after any redirects.
pub type NavigationResult = Result<(dom::Element, css_parser::Stylesheet, images::ImageCache, Vec<PendingImage>, String), String>;

/// Milestone 21: the receiving half of a background page navigation —
/// mirrors `network::FetchHandle`'s exact shape/contract (never
/// blocks; one `Some` is the final answer, calling `try_recv` again
/// after that returns `None` forever).
pub struct NavigationHandle {
    receiver: std::sync::mpsc::Receiver<NavigationResult>,
}

impl NavigationHandle {
    pub fn try_recv(&self) -> Option<NavigationResult> {
        self.receiver.try_recv().ok()
    }
}

/// Milestone 21: runs `fetch_page` (parse → linked stylesheets/images,
/// all real network I/O) on a genuine background OS thread — same
/// `std::thread::spawn` + `mpsc::channel` shape as
/// `network::NetworkContext::fetch_async`, so navigating to a new page
/// never blocks the still-rendering, still-interactive OLD page. The
/// new page gets its OWN fresh `ImageCache` (see `NavigationResult`),
/// built entirely on the background thread — no `Arc<Mutex<..>>`
/// needed, the whole result just crosses the channel once, complete.
///
/// Takes the shared `NetworkContext` BY VALUE (a cheap `Arc` clone) so
/// the background thread owns its handle: the new page load shares the
/// profile's cookies and cache with everything else.
///
/// `initiator` is the page the navigation came from (see [`fetch_page`]).
#[cfg(test)]
pub fn navigate_async(url: &str, network: network::NetworkContext, initiator: Option<network::Initiator>) -> NavigationHandle {
    navigate_async_with(url, network, initiator, ImageLoading::BeforeFirstFrame)
}

/// [`navigate_async`], with the page's images loaded as `loading` says:
/// with [`ImageLoading::Progressive`] the result carries the image requests
/// still in flight.
pub fn navigate_async_with(
    url: &str,
    network: network::NetworkContext,
    initiator: Option<network::Initiator>,
    loading: ImageLoading,
) -> NavigationHandle {
    use diagnostics::{metrics, tracing};
    let (sender, receiver) = std::sync::mpsc::channel();
    let url = url.to_string();
    metrics::NAVIGATIONS_STARTED.inc();
    tracing::info!(target: "engine::navigation", url = %network::loggable_url(&url), "navigation started");
    std::thread::spawn(move || {
        let mut image_cache = images::ImageCache::new();
        let result = fetch_page_progressive(&url, &mut image_cache, &network, initiator.as_ref()).map(|(root, sheet, pending, landed)| {
            let pending = match loading {
                ImageLoading::Progressive => pending,
                ImageLoading::BeforeFirstFrame => {
                    wait_for_images(pending, &mut image_cache);
                    Vec::new()
                }
            };
            (root, sheet, image_cache, pending, landed)
        });
        match &result {
            Ok(_) => {
                metrics::NAVIGATIONS_FINISHED.inc();
                tracing::info!(target: "engine::navigation", url = %network::loggable_url(&url), "navigation finished");
            }
            Err(e) => {
                metrics::NAVIGATIONS_FAILED.inc();
                tracing::warn!(target: "engine::navigation", url = %network::loggable_url(&url), error = %e, "navigation failed");
            }
        }
        // A `send` failure just means the caller stopped caring (e.g.
        // navigated away AGAIN before this one finished) — matching
        // `fetch_async`'s exact same reasoning, not an error to report.
        let _ = sender.send(result);
        // Milestone 38b (ADR 020): the page lands it at its next tick.
        if let Some(notify) = network.notifier() {
            notify();
        }
    });
    NavigationHandle { receiver }
}

/// Concatenates every inline `<style>` element's text, in document
/// order.
pub fn collect_style_text(element: dom::ElementRef<'_>, out: &mut String) {
    for child in element.children() {
        match child {
            dom::ChildRef::Element(el) if el.is(dom::atoms::STYLE) => {
                for grandchild in el.children() {
                    if let dom::ChildRef::Text(text) = grandchild {
                        out.push_str(text);
                        out.push('\n');
                    }
                }
            }
            dom::ChildRef::Element(el) => collect_style_text(el, out),
            dom::ChildRef::Text(_) => {}
        }
    }
}

/// `<link rel="stylesheet" href="...">` hrefs, in document order.
pub fn collect_stylesheet_links(element: dom::ElementRef<'_>, out: &mut Vec<String>) {
    if element.is(dom::atoms::LINK) {
        let is_stylesheet = element.attr_atom(dom::atoms::REL).map(|r| r.eq_ignore_ascii_case("stylesheet")).unwrap_or(false);
        if is_stylesheet {
            if let Some(href) = element.attr_atom(dom::atoms::HREF) {
                out.push(href.to_string());
            }
        }
    }
    for child in element.children() {
        if let dom::ChildRef::Element(el) = child {
            collect_stylesheet_links(el, out);
        }
    }
}

/// Rewrites every `<img>`'s `src` to an absolute URL (resolved against
/// `base_url`) in place, and pushes each resulting absolute URL onto
/// `out`. `data:` URIs and un-resolvable values are left untouched and
/// not collected — nothing to fetch there.
pub fn rewrite_img_srcs_to_absolute(element: &mut dom::Element, base_url: &str, out: &mut Vec<String>) {
    if element.tag == "img" {
        if let Some(src) = element.attr_atom(dom::atoms::SRC).map(str::to_string) {
            if !src.starts_with("data:") {
                if let Some(absolute) = network::resolve_url(base_url, &src) {
                    element.set_attr("src", absolute.clone());
                    out.push(absolute);
                }
            }
        }
    }
    for child in &mut element.children {
        if let dom::Node::Element(el) = child {
            rewrite_img_srcs_to_absolute(el, base_url, out);
        }
    }
}

/// Rewrites every `url(...)` reference in a stylesheet's text to an
/// absolute URL (resolved against `base_url`), returning the rewritten
/// text and pushing each resulting absolute URL onto `out`.
///
/// Only URLs of `background`/`background-image` declarations are pushed onto
/// `out` (fetched); every `url(...)` is still made absolute.
///
/// Handles `url(x)`, `url("x")`, `url('x')`, and surrounding
/// whitespace — the same set `style::parse_url` accepts. `data:` URIs
/// and un-resolvable values pass through unchanged. A `url(` with no
/// closing `)` ends the scan (the malformed tail is emitted verbatim).
/// Whether the CSS text just before a `url(` belongs to a declaration of a
/// property the engine paints as an image (`background`,
/// `background-image`): the property name is what precedes the last `:`
/// after the last `;`, `{` or `}`.
fn declares_background_image(before_url: &str) -> bool {
    let declaration_start = before_url.rfind([';', '{', '}']).map_or(0, |i| i + 1);
    let declaration = &before_url[declaration_start..];
    let Some(colon) = declaration.find(':') else { return false };
    let property = declaration[..colon].trim().to_ascii_lowercase();
    property == "background" || property == "background-image"
}

pub fn rewrite_css_image_urls(css: &str, base_url: &str, out: &mut Vec<String>) -> String {
    let mut result = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find("url(") {
        result.push_str(&rest[..pos + 4]);
        rest = &rest[pos + 4..];
        let Some(end) = rest.find(')') else { break };
        let raw = &rest[..end];
        let unquoted = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if unquoted.is_empty() || unquoted.starts_with("data:") {
            result.push_str(raw);
        } else if let Some(absolute) = network::resolve_url(base_url, unquoted) {
            result.push_str(&absolute);
            // Rewritten to absolute everywhere, but FETCHED only for the
            // properties that paint an image. Every `url()` used to be
            // fetched and decoded as an image — including `@font-face` web
            // fonts (6 MB of the conformance corpus), `cursor`, `mask-image`.
            if declares_background_image(&result[..result.len() - absolute.len() - 4]) {
                out.push(absolute);
            }
        } else {
            result.push_str(raw);
        }
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}


#[cfg(test)]
mod decode_tests {
    use super::*;

    /// Milestone 34: collecting arrived images never waits for a decode —
    /// one still decoding stays pending, and is filed by a later call.
    #[test]
    fn a_tick_does_not_wait_for_an_image_to_decode() {
        let mut cache = images::ImageCache::new();
        cache.mark_pending("http://a/x.png");
        let (decoder, decoded) = std::sync::mpsc::channel();
        let mut pending = vec![PendingImage { url: "http://a/x.png".into(), state: ImageState::Decoding(decoded) }];
        assert!(take_arrived_images(&mut pending, &mut cache, None).is_empty(), "still decoding: nothing filed, nothing waited for");
        assert_eq!(pending.len(), 1);
        assert!(cache.is_pending("http://a/x.png"));

        decoder.send(Some(images::DecodedImage { width: 1, height: 1, rgba: std::sync::Arc::new(vec![0, 0, 0, 255]) })).unwrap();
        assert_eq!(take_arrived_images(&mut pending, &mut cache, None), vec!["http://a/x.png".to_string()]);
        assert!(pending.is_empty() && cache.get("http://a/x.png").is_some());
    }

    #[test]
    fn bytes_that_do_not_decode_finish_the_image_as_missing() {
        let result = decode_in_background(Some(b"not an image".to_vec()), None).recv().unwrap();
        assert!(result.is_none());
        assert!(decode_in_background(None, None).recv().unwrap().is_none(), "no body: nothing to decode");
    }
}
