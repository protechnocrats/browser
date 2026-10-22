//! Milestone 38b (ADR 020), through the public API: every kind of background
//! work a page waits for — the navigation, a script `fetch`, an
//! `XMLHttpRequest`, an image (fetch, then decode) and a web font (fetch,
//! then decode) — wakes the page when it finishes, with no polling in
//! between, on a thread and in a renderer process.

use engine::{Engine, EngineConfig, EngineEvent, ImageLoading, InProcessLauncher, Isolation, LoadRequest, PageCommand, PageParams, Viewport};
use rendering::DisplayItem;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the server holds every subresource back.
const DELAY: Duration = Duration::from_millis(600);
const SERIF: &[u8] = include_bytes!("../../../test-fonts/DejaVuSerif.ttf");
const PNG: &[u8] = include_bytes!("../../images/tests/fixtures/transparent.png");

const PAGE: &str = "<html><head><style>\
    @font-face { font-family: Brand; src: url(/font.ttf) format('truetype'); }\
    p { font-family: Brand, sans-serif; font-size: 20px }\
    #f, #x { width: 30px; height: 30px; background-color: #000000 }\
    </style></head><body><p>Wide Mm</p><div id=f></div><div id=x></div><img src=\"/img.png\" width=10 height=10>\
    <script>\
    fetch('/data').then(function (r) { return r.text(); }).then(function () { document.getElementById('f').style.backgroundColor = '#00ff00'; });\
    var x = new XMLHttpRequest(); x.open('GET', '/data');\
    x.onload = function () { document.getElementById('x').style.backgroundColor = '#0000ff'; }; x.send();\
    </script></body></html>";

/// The page at once; everything else after [`DELAY`].
fn serve() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap_or(0);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body: Vec<u8> = match path.as_str() {
                    "/" => PAGE.as_bytes().to_vec(),
                    "/font.ttf" => SERIF.to_vec(),
                    "/img.png" => PNG.to_vec(),
                    _ => b"ok".to_vec(),
                };
                if path != "/" {
                    std::thread::sleep(DELAY);
                }
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

#[derive(Default)]
struct Seen {
    first_glyph_xs: Option<Vec<f32>>,
    font: bool,
    image: bool,
    fetch: bool,
    xhr: bool,
}

impl Seen {
    fn frame(&mut self, items: &[DisplayItem]) {
        let xs: Option<Vec<f32>> = items.iter().find_map(|i| match i {
            DisplayItem::Glyphs { instances, .. } => Some(instances.iter().map(|g| g.x).collect()),
            _ => None,
        });
        if let Some(xs) = xs {
            match &self.first_glyph_xs {
                None => self.first_glyph_xs = Some(xs),
                // Serif metrics differ from the sans fallback it started with.
                Some(first) => self.font |= *first != xs,
            }
        }
        self.image |= items.iter().any(|i| matches!(i, DisplayItem::Image { .. }));
        let rect = |r: u8, g: u8, b: u8| items.iter().any(|i| matches!(i, DisplayItem::Rect { color, .. } if (color.r, color.g, color.b, color.a) == (r, g, b, 255)));
        self.fetch |= rect(0, 255, 0);
        self.xhr |= rect(0, 0, 255);
    }

    fn all(&self) -> bool {
        self.font && self.image && self.fetch && self.xhr
    }
}

fn everything_wakes_the_page(isolation: Isolation) {
    let url = serve();
    let network = engine::NetworkContext::new();
    let mut engine = Engine::new(EngineConfig { network: network.clone(), image_loading: ImageLoading::Progressive, isolation, ..EngineConfig::default() });
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&woken);
    engine.set_waker(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    // Script may fetch: the network grant a profile gives a web page.
    let grants = capability::PageGrants { network: Some(capability::Authority::new().network(network)), storage: None };
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)).with_grants(grants));
    engine.send(page, PageCommand::Load(LoadRequest::Url(url)));

    // A host that updates only when woken or when the engine says a page is
    // due — as the desktop shell does.
    let started = Instant::now();
    let mut seen = Seen::default();
    let mut updates = 0;
    while !seen.all() {
        assert!(started.elapsed() < Duration::from_secs(20), "never finished loading: font {} image {} fetch {} xhr {}", seen.font, seen.image, seen.fetch, seen.xhr);
        let due = engine.next_wake().is_some_and(|at| at <= Instant::now());
        if woken.swap(0, Ordering::SeqCst) == 0 && !due && updates > 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        updates += 1;
        for event in engine.update(Instant::now()) {
            match event {
                EngineEvent::Frame { scene, .. } => seen.frame(&scene.items),
                EngineEvent::PageCrashed { reason, .. } => panic!("crashed: {reason}"),
                _ => {}
            }
        }
    }
    assert!(started.elapsed() >= DELAY, "the server held everything back");
    // Polled every 8 ms (Milestone 30), the wait alone would have taken at
    // least 75 updates; woken, it takes one or two per thing that finished.
    assert!(updates <= 40, "{updates} updates");
}

#[test]
fn every_kind_of_background_work_wakes_a_page_on_a_thread() {
    everything_wakes_the_page(Isolation::Threads);
}

#[test]
fn every_kind_of_background_work_wakes_a_page_in_a_renderer_process() {
    everything_wakes_the_page(Isolation::Processes(Arc::new(InProcessLauncher)));
}
