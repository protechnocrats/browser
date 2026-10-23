//! What the browser process gives a renderer of a cross-origin response
//! (Milestone 41, ADR 025 §5).
//!
//! A renderer states what each request is for (`network::Destination`), and
//! a compromised one can state anything: call a script's request an image
//! load to get round its page's missing network grant, or read a response
//! the same-origin policy keeps from script. So the browser does not take
//! the label on trust. It hands over a cross-origin response only if the
//! request's purpose can use it:
//!
//! - **script's requests** (`fetch`, XHR — and anything labelled `script`
//!   or `other`, which the loader never sends) and **fonts** (anonymous
//!   CORS requests, CSS Fonts 4 §4.9) only when CORS lets the document
//!   read it (`origin::cors_response_readable`, the rule script applies
//!   itself);
//! - **images** only when the body is one (PNG, JPEG, GIF, WebP — what the
//!   engine decodes); anything else could not have been drawn anyway;
//! - **stylesheets** unless it is served as HTML, JSON or XML — what no
//!   stylesheet is, and what a page's private data usually is.
//!
//! Otherwise the renderer gets the status and the URL, with no headers and
//! no body — what a page with an honest renderer sees of such a response
//! (a broken image, a stylesheet that applies nothing, a script request that
//! fails its own CORS check). Same-origin responses, and navigations (the
//! renderer's own document), are handed over whole.

use network::{Destination, FetchResponse, RequestContext};
use origin::Origin;

/// Why `response` is withheld from the renderer that asked for it under
/// `ctx`, or `None` if it may have it.
pub(crate) fn withheld(ctx: &RequestContext, response: &FetchResponse) -> Option<&'static str> {
    if ctx.is_navigation {
        return None;
    }
    // A request without an initiator is a typed navigation (handled above);
    // the host builds every other request's initiator itself.
    let initiator = ctx.initiator.as_ref()?;
    let from = Origin::parse(&response.final_url);
    if initiator.origin.same_origin(&from) {
        return None;
    }
    match ctx.destination {
        Destination::Image => (!response.body.is_empty() && !is_image(&response.body)).then_some("a cross-origin image request's response is not an image"),
        Destination::Stylesheet => is_private_data(response).then_some("a cross-origin stylesheet request's response is HTML, JSON or XML"),
        Destination::Document | Destination::Font | Destination::Script | Destination::XmlHttpRequest | Destination::Other => {
            let readable = origin::cors_response_readable(
                &initiator.origin,
                &from,
                ctx.credentials_apply_to(&from),
                response.header("Access-Control-Allow-Origin"),
                response.header("Access-Control-Allow-Credentials"),
            );
            (!readable).then_some("a cross-origin response its document may not read")
        }
    }
}

/// `response` as the renderer may see it: status and URL, nothing else.
pub(crate) fn emptied(response: FetchResponse) -> FetchResponse {
    FetchResponse { status: response.status, final_url: response.final_url, headers: Vec::new(), body: Vec::new() }
}

/// Whether a request labelled `destination` is one the document loader
/// makes: what a page without the network capability may still ask for
/// (its own document, stylesheets, images and fonts — never script's).
pub(crate) fn is_loader_request(destination: Destination) -> bool {
    matches!(destination, Destination::Document | Destination::Stylesheet | Destination::Image | Destination::Font)
}

/// The image formats `images::decode_auto` reads, by their magic bytes.
fn is_image(body: &[u8]) -> bool {
    body.starts_with(b"\x89PNG\r\n\x1a\n")
        || body.starts_with(&[0xFF, 0xD8, 0xFF])
        || body.starts_with(b"GIF87a")
        || body.starts_with(b"GIF89a")
        || (body.len() >= 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP")
}

/// Served as HTML, JSON or XML (by `Content-Type`, the way the ORB
/// standard's list starts) — never a stylesheet.
fn is_private_data(response: &FetchResponse) -> bool {
    let Some(content_type) = response.header("Content-Type") else { return false };
    let essence = network::parse_content_type(content_type).mime;
    essence == "text/html"
        || essence == "text/xml"
        || essence == "application/xml"
        || essence == "application/json"
        || essence == "text/json"
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use network::{Credentials, Initiator};

    fn ctx(destination: Destination, credentials: Credentials) -> RequestContext {
        RequestContext::subresource_by(Initiator::for_url("https://news.example/")).with_destination(destination).with_credentials(credentials)
    }

    fn response(url: &str, headers: &[(&str, &str)], body: &[u8]) -> FetchResponse {
        FetchResponse {
            status: 200,
            final_url: url.into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.to_vec(),
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n....";
    const SECRET: &[u8] = br#"{"balance": 1000}"#;

    /// A mislabelled request gets nothing its label cannot use: a script's
    /// request called an image load, or a stylesheet, reads no JSON.
    #[test]
    fn a_mislabelled_request_gets_nothing_its_label_cannot_use() {
        let json = [("Content-Type", "application/json")];
        let bank = "https://bank.example/account";
        for destination in [Destination::Image, Destination::Stylesheet, Destination::XmlHttpRequest, Destination::Other, Destination::Script, Destination::Font] {
            assert!(withheld(&ctx(destination, Credentials::Include), &response(bank, &json, SECRET)).is_some(), "{destination:?}");
        }
        // What each label is for still arrives.
        assert_eq!(withheld(&ctx(Destination::Image, Credentials::Include), &response(bank, &[], PNG)), None);
        assert_eq!(withheld(&ctx(Destination::Stylesheet, Credentials::Include), &response(bank, &[("Content-Type", "text/css")], b"p{}")), None);
        assert_eq!(withheld(&ctx(Destination::Image, Credentials::Include), &response(bank, &[], b"")), None, "an empty body is nothing to withhold");
    }

    #[test]
    fn cors_decides_for_scripts_requests_and_fonts() {
        let open = [("Access-Control-Allow-Origin", "*")];
        let api = "https://api.example/data";
        assert_eq!(withheld(&ctx(Destination::XmlHttpRequest, Credentials::SameOrigin), &response(api, &open, SECRET)), None);
        assert_eq!(withheld(&ctx(Destination::Font, Credentials::SameOrigin), &response(api, &open, b"wOFF")), None);
        // `*` is not enough once cookies went with it.
        assert!(withheld(&ctx(Destination::XmlHttpRequest, Credentials::Include), &response(api, &open, SECRET)).is_some());
        let named = [("Access-Control-Allow-Origin", "https://news.example"), ("Access-Control-Allow-Credentials", "true")];
        assert_eq!(withheld(&ctx(Destination::XmlHttpRequest, Credentials::Include), &response(api, &named, SECRET)), None);
    }

    #[test]
    fn same_origin_responses_and_navigations_are_never_withheld() {
        let json = [("Content-Type", "application/json")];
        assert_eq!(withheld(&ctx(Destination::Image, Credentials::Include), &response("https://news.example/me", &json, SECRET)), None);
        let navigation = RequestContext::navigation_by(Initiator::for_url("https://news.example/"));
        assert_eq!(withheld(&navigation, &response("https://bank.example/", &json, SECRET)), None);
        let emptied = emptied(response("https://bank.example/", &json, SECRET));
        assert!(emptied.body.is_empty() && emptied.headers.is_empty() && emptied.status == 200);
    }
}
