//! Milestone 35: `@font-face` web fonts, end to end — fetched as anonymous
//! CORS requests, used for the page's text, refused cross-origin without
//! permission, and never seen by another page.

use engine::{Engine, EngineConfig, EngineEvent, ImageLoading, LoadRequest, PageCommand, PageParams, Viewport};
use rendering::DisplayItem;
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);
const SERIF: &[u8] = include_bytes!("../../../test-fonts/DejaVuSerif.ttf");

/// A server answering every request with `body`, and `extra` headers.
fn serve(body: Vec<u8>, extra: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let _ = write!(stream, "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The x positions of the glyphs of the page's first text run.
fn glyph_xs(font_url: &str) -> Vec<f32> {
    let page_html = format!(
        "<html><head><style>@font-face {{ font-family: Brand; src: url({font_url}) format('truetype'); }} p {{ font-family: Brand, sans-serif; font-size: 30px }}</style></head><body><p>Wide Mm</p></body></html>"
    );
    let page_base = serve(page_html.into_bytes(), "");
    let mut engine = Engine::new(EngineConfig { image_loading: ImageLoading::BeforeFirstFrame, ..EngineConfig::default() });
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 200.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{page_base}/"))));
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the page never drew");
        for e in engine.update_blocking(Instant::now(), WAIT) {
            if let EngineEvent::Frame { scene, .. } = e {
                if let Some(xs) = scene.items.iter().find_map(|i| match i {
                    DisplayItem::Glyphs { instances, .. } => Some(instances.iter().map(|g| g.x).collect::<Vec<_>>()),
                    _ => None,
                }) {
                    return xs;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_web_font_is_used_and_a_cross_origin_one_needs_cors() {
    let allowed = serve(SERIF.to_vec(), "Access-Control-Allow-Origin: *\r\n");
    let refused = serve(SERIF.to_vec(), "");
    let with_font = glyph_xs(&format!("{allowed}/serif.ttf"));
    let without = glyph_xs(&format!("{refused}/serif.ttf"));
    let fallback = glyph_xs("http://127.0.0.1:9/missing.ttf");
    assert_eq!(without, fallback, "a cross-origin font without CORS permission is not used");
    assert_ne!(with_font, fallback, "the permitted web font draws the text (serif metrics differ from the sans fallback)");
}

#[test]
fn bytes_that_are_not_a_font_fall_back_quietly() {
    let junk = serve(b"<html>not a font</html>".to_vec(), "Access-Control-Allow-Origin: *\r\n");
    let xs = glyph_xs(&format!("{junk}/font.ttf"));
    assert_eq!(xs, glyph_xs("http://127.0.0.1:9/missing.ttf"));
}

/// Serves `routes` by path (404 otherwise).
fn serve_routes(routes: Vec<(&'static str, Vec<u8>)>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let routes = routes.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn first_glyph_xs(base: &str) -> Vec<f32> {
    let mut engine = Engine::new(EngineConfig { image_loading: ImageLoading::BeforeFirstFrame, ..EngineConfig::default() });
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 200.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/site/pages/index.html"))));
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the page never drew");
        for e in engine.update_blocking(Instant::now(), WAIT) {
            if let EngineEvent::Frame { scene, .. } = e {
                if let Some(xs) = scene.items.iter().find_map(|i| match i {
                    DisplayItem::Glyphs { instances, .. } => Some(instances.iter().map(|g| g.x).collect::<Vec<_>>()),
                    _ => None,
                }) {
                    return xs;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A stylesheet's `url()`s are relative to the stylesheet: a font in
/// `../fonts/` next to `/css/site.css` is found (resolved against the page,
/// `/site/pages/`, it would be `/site/fonts/`, which does not exist).
#[test]
fn a_font_url_in_a_linked_stylesheet_is_relative_to_the_stylesheet() {
    let page = b"<html><head><link rel=stylesheet href=/css/site.css></head><body><p>Wide Mm</p></body></html>".to_vec();
    let css = b"@font-face { font-family: Brand; src: url('../fonts/brand.ttf') format('truetype'); } p { font-family: Brand, sans-serif; font-size: 30px }".to_vec();
    let with_font = serve_routes(vec![("/site/pages/index.html", page.clone()), ("/css/site.css", css.clone()), ("/fonts/brand.ttf", SERIF.to_vec())]);
    let without = serve_routes(vec![("/site/pages/index.html", page), ("/css/site.css", css)]);
    assert_ne!(first_glyph_xs(&with_font), first_glyph_xs(&without), "the font was found relative to the stylesheet");
}
