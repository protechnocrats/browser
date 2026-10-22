//! Milestone 35: a document's web fonts (`@font-face`) — fetched through the
//! page's network context as anonymous CORS requests, decoded off the page
//! thread, and handed to the page's own font set (never another page's).
//!
//! Zero-trust: a cross-origin font needs `Access-Control-Allow-Origin` for
//! the page (CSS Fonts 4 §4.9); the bytes are sniffed and bounded before
//! decompression and validated as a font (`text::decode_web_font`,
//! `text::Font::add_web_font`). A document's fonts are capped in number.

use network::{FetchHandle, NetworkContext, RequestContext};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

/// `@font-face` rules a document may load fonts for.
pub const MAX_WEB_FONTS: usize = 64;

/// A web font ready to join the page's font set.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadyFont {
    pub family: String,
    pub weight: u16,
    pub style: text::FontStyle,
    pub bytes: Vec<u8>,
}

enum State {
    Fetching(FetchHandle),
    Decoding(Receiver<Option<Vec<u8>>>),
}

/// One `@font-face` being loaded.
pub struct WebFontLoad {
    family: String,
    weight: u16,
    style: text::FontStyle,
    url: String,
    page: origin::Origin,
    state: State,
}

/// Starts fetching the fonts of `sheet`'s `@font-face` rules, for a document
/// at `page_url`. Each rule's first `url()` source in a format we can read.
pub fn start(sheet: &css_parser::Stylesheet, page_url: Option<&str>, network: &NetworkContext) -> Vec<WebFontLoad> {
    let Some(page_url) = page_url else { return Vec::new() };
    let page = origin::Origin::parse(page_url);
    // Anonymous CORS: cookies only to the page's own origin.
    let context = RequestContext { credentials: network::Credentials::SameOrigin, ..RequestContext::subresource_of(page_url).with_destination(network::Destination::Font) };
    let mut loads = Vec::new();
    for rule in sheet.font_faces.iter() {
        if loads.len() >= MAX_WEB_FONTS {
            diagnostics::guard(diagnostics::Guard::FontSizeClamped, "too many @font-face rules; the rest are not loaded");
            break;
        }
        let Some(family) = rule.family.as_deref().map(|f| f.trim().trim_matches(|c| c == '"' || c == '\'').to_string()).filter(|f| !f.is_empty()) else { continue };
        let source = rule.sources.iter().find(|s| {
            s.url.is_some() && s.format.as_deref().is_none_or(|f| matches!(f.to_ascii_lowercase().trim_matches(|c| c == '"' || c == '\''), "woff" | "woff2" | "truetype" | "opentype" | "woff2-variations" | "truetype-variations" | "opentype-variations"))
        });
        let Some(url) = source.and_then(|s| s.url.clone()).and_then(|u| network::resolve_url(page_url, &u)) else { continue };
        let weight = rule.weight.as_deref().and_then(|w| style::text::parse_font_weight(w.split_whitespace().next().unwrap_or(w), 400)).unwrap_or(400);
        let style = match rule.style.as_deref().and_then(style::text::parse_font_style) {
            Some(style::FontStyleKind::Italic) => text::FontStyle::Italic,
            Some(style::FontStyleKind::Oblique) => text::FontStyle::Oblique,
            _ => text::FontStyle::Normal,
        };
        let handle = network.fetch_async_with(&url, &context);
        loads.push(WebFontLoad { family, weight, style, url, page: page.clone(), state: State::Fetching(handle) });
    }
    loads
}

/// The font file a response holds, if the page may use it.
fn accept(load: &WebFontLoad, response: network::FetchResponse) -> Option<Vec<u8>> {
    if !(200..300).contains(&response.status) {
        return None;
    }
    let from = origin::Origin::parse(&response.final_url);
    if !origin::cors_response_readable(&load.page, &from, false, response.header("access-control-allow-origin"), None) {
        diagnostics::guard(diagnostics::Guard::RendererRequestRefused, "a cross-origin web font without CORS permission was not used");
        return None;
    }
    Some(response.body)
}

fn decode_in_background(bytes: Vec<u8>, notify: Option<network::Notify>) -> Receiver<Option<Vec<u8>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    rayon::spawn(move || {
        let out = std::panic::catch_unwind(|| text::decode_web_font(&bytes).ok()).ok().flatten();
        let _ = tx.send(out);
        if let Some(notify) = notify {
            notify();
        }
    });
    rx
}

fn ready(load: &WebFontLoad, bytes: Vec<u8>) -> ReadyFont {
    ReadyFont { family: load.family.clone(), weight: load.weight, style: load.style, bytes }
}

/// Moves fonts along without waiting: arrived ones start decoding, decoded
/// ones are returned. The rest stay pending.
pub fn poll(pending: &mut Vec<WebFontLoad>, notify: Option<&network::Notify>) -> Vec<ReadyFont> {
    let mut out = Vec::new();
    pending.retain_mut(|load| {
        if let State::Fetching(handle) = &load.state {
            let Some(result) = handle.try_recv() else { return true };
            match result.ok().and_then(|r| accept(load, r)) {
                Some(bytes) => load.state = State::Decoding(decode_in_background(bytes, notify.cloned())),
                None => return false,
            }
        }
        let State::Decoding(rx) = &load.state else { return true };
        match rx.try_recv() {
            Ok(Some(bytes)) => {
                out.push(ready(load, bytes));
                false
            }
            Ok(None) | Err(TryRecvError::Disconnected) => {
                diagnostics::guard(diagnostics::Guard::RendererRequestRefused, &format!("web font {} did not decode", load.url));
                false
            }
            Err(TryRecvError::Empty) => true,
        }
    });
    out
}

/// Waits (until `deadline`) for every font — the "before first frame" load.
pub fn wait(pending: Vec<WebFontLoad>, deadline: Instant) -> Vec<ReadyFont> {
    let mut out = Vec::new();
    for load in pending {
        let State::Fetching(handle) = &load.state else { continue };
        if let Some(bytes) = handle.wait_until(deadline).and_then(|r| r.ok()).and_then(|r| accept(&load, r)) {
            if let Ok(font) = text::decode_web_font(&bytes) {
                out.push(ready(&load, font));
            }
        }
    }
    out
}
