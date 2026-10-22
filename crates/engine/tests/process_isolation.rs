//! ADR 016, through the public API: a page in a renderer "process" (the
//! in-process launcher runs the real protocol over OS pipes) draws exactly
//! what a page thread draws, reaches the network only through the host, and
//! a failing renderer is contained. `crates/renderer/tests` repeats the key
//! cases with real, sandboxed processes.

use engine::{
    Engine, EngineConfig, EngineEvent, EngineInput, ImageLoading, InProcessLauncher, Isolation, LoadRequest, NetworkContext, PageCommand, PageId,
    PageParams, RendererConnection, RendererControl, RendererLauncher, Viewport,
};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn engine(isolation: Isolation, network: NetworkContext) -> Engine {
    // Milestone 35: real fonts from a source — in process mode the renderer
    // gets them from the host over IPC.
    let fonts: std::sync::Arc<dyn text::FontSource> =
        std::sync::Arc::new(text::DirectoryFonts::scan(&[std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fonts"))]));
    Engine::new(EngineConfig { network, waker: None, image_loading: ImageLoading::BeforeFirstFrame, isolation, fonts: Some(fonts) })
}

/// Updates until `done` says so, returning every event seen.
fn run_until(engine: &mut Engine, now: &mut Instant, mut done: impl FnMut(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let deadline = Instant::now() + WAIT;
    let mut all = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "timed out; events so far: {all:?}");
        *now += Duration::from_millis(16);
        all.extend(engine.update_blocking(*now, WAIT));
        if done(&all) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn last_scene(events: &[EngineEvent], page: PageId) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        EngineEvent::Frame { page: p, scene, .. } if *p == page => Some(engine::scene_to_json(scene)),
        _ => None,
    })
}

const PAGE: &str = "<html><head><style>#h { position: fixed; top: 0; height: 20px; width: 100%; background: #336699; } .row { height: 30px; border: 1px solid #cccccc; } .box { opacity: 0.5; box-shadow: 2px 2px 3px #000000; }</style></head>\
    <body><div id=h>header</div><div class=box>shadowed</div><p><b>bold</b> <i>italic</i> नमस्ते 世界 שלום</p>\
    <div id=list></div>\
    <script>for (var i = 0; i < 60; i++) { var d = document.createElement('div'); d.setAttribute('class', 'row'); d.textContent = 'row ' + i; document.getElementById('list').appendChild(d); }</script>\
    </body></html>";

/// Loads `PAGE`, scrolls, and returns the scene of each step.
fn scenes(isolation: Isolation) -> Vec<String> {
    let mut engine = engine(isolation, NetworkContext::new());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    let mut now = Instant::now();
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: PAGE.into(), url: Some("https://example.test/".into()) }));
    let mut out = Vec::new();
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) && last_scene(e, page).is_some());
    out.push(last_scene(&events, page).unwrap());
    for dy in [120.0, 500.0, -60.0] {
        engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy }));
        let events = run_until(&mut engine, &mut now, |e| last_scene(e, page).is_some());
        out.push(last_scene(&events, page).unwrap());
    }
    out
}

#[test]
fn a_page_in_a_renderer_process_draws_exactly_what_a_page_thread_draws() {
    let threads = scenes(Isolation::Threads);
    let processes = scenes(Isolation::Processes(Arc::new(InProcessLauncher)));
    assert_eq!(threads.len(), processes.len());
    for (i, (a, b)) in threads.iter().zip(&processes).enumerate() {
        assert_eq!(a, b, "step {i}: the scene rebuilt from the wire differs");
    }
}

/// Serves a page that sets a cookie, and an image.
fn serve() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let png = include_bytes!("../../images/tests/fixtures/quadrants.png");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
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
                let (headers, body): (&str, &[u8]) = if request.contains("/img.png") {
                    ("", png)
                } else {
                    ("Set-Cookie: session=abc; Path=/\r\n", b"<html><body><p>networked</p><img src=\"/img.png\"></body></html>")
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_renderer_reaches_the_network_only_through_the_host() {
    let base = serve();
    let network = NetworkContext::new();
    let mut engine = engine(Isolation::Processes(Arc::new(InProcessLauncher)), network.clone());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    let mut now = Instant::now();
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/index.html"))));
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) && last_scene(e, page).is_some());
    let scene = events.iter().rev().find_map(|e| if let EngineEvent::Frame { scene, .. } = e { Some(scene.clone()) } else { None }).unwrap();
    assert_eq!(scene.images.len(), 1, "the image crossed from the host to the renderer and back");
    // The cookie is in the HOST's jar: the host made the request.
    let host = base.trim_start_matches("http://").split(':').next().unwrap().to_string();
    assert_eq!(network.cookie_jar().header_for(&host, "/", false).as_deref(), Some("session=abc"));
}

#[test]
fn a_panic_in_a_renderer_is_that_pages_crash() {
    let mut engine = engine(Isolation::Processes(Arc::new(InProcessLauncher)), NetworkContext::new());
    let crashing = engine.create_page(PageParams::new(Viewport::new(200.0, 100.0)));
    let healthy = engine.create_page(PageParams::new(Viewport::new(200.0, 100.0)));
    let mut now = Instant::now();
    engine.send(crashing, PageCommand::SimulateCrash);
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })));
    assert!(events.iter().any(|e| matches!(e, EngineEvent::PageCrashed { page, .. } if *page == crashing)));
    engine.send(healthy, PageCommand::Load(LoadRequest::Html { html: "<html><body>fine</body></html>".into(), url: None }));
    run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { page, .. } if *page == healthy)));
}

/// A "renderer" that sends whatever it is given — a stand-in for a
/// compromised one.
struct Hostile(Vec<u8>);

struct NoControl;
impl RendererControl for NoControl {
    fn terminate(&mut self) {}
    fn exit_status(&mut self) -> Option<String> {
        None
    }
    fn pid(&self) -> Option<u32> {
        None
    }
}

impl RendererLauncher for Hostile {
    fn launch(&self) -> std::io::Result<RendererConnection> {
        Ok(RendererConnection { to_renderer: Box::new(std::io::sink()), from_renderer: Box::new(std::io::Cursor::new(self.0.clone())), control: Box::new(NoControl) })
    }
}

#[test]
fn a_renderer_that_sends_garbage_is_ended_and_the_host_carries_on() {
    let mut claims_a_huge_message = (u32::MAX).to_le_bytes().to_vec();
    claims_a_huge_message.extend_from_slice(&[0; 16]);
    let mut bad_tag = 3u32.to_le_bytes().to_vec();
    bad_tag.extend_from_slice(&[250, 1, 2]);
    for (label, bytes) in [("a huge length", claims_a_huge_message), ("an unknown message", bad_tag), ("silence, then the pipe closes", vec![])] {
        let mut engine = engine(Isolation::Processes(Arc::new(Hostile(bytes))), NetworkContext::new());
        let page = engine.create_page(PageParams::new(Viewport::new(200.0, 100.0)));
        let mut now = Instant::now();
        let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })));
        assert!(events.iter().any(|e| matches!(e, EngineEvent::PageCrashed { page: p, .. } if *p == page)), "{label}");
        assert!(events.iter().all(|e| !matches!(e, EngineEvent::Frame { .. })), "{label}: nothing drawn from it");
    }
}

#[test]
fn closing_a_process_page_ends_it_quietly() {
    let mut engine = engine(Isolation::Processes(Arc::new(InProcessLauncher)), NetworkContext::new());
    let page = engine.create_page(PageParams::new(Viewport::new(200.0, 100.0)));
    let mut now = Instant::now();
    run_until(&mut engine, &mut now, |e| last_scene(e, page).is_some());
    engine.close_page(page);
    now += Duration::from_millis(16);
    let events = engine.update_blocking(now, Duration::from_millis(200));
    assert!(events.is_empty(), "no crash reported for a page closed on purpose: {events:?}");
}

/// A new page draws its blank first frame without being told anything (the
/// page is due at creation) — in both modes. (Found while testing process
/// mode: the first `update` used to cancel it, in thread mode too.)
#[test]
fn a_new_page_draws_its_blank_first_frame_unprompted() {
    for isolation in [Isolation::Threads, Isolation::Processes(Arc::new(InProcessLauncher))] {
        let mut engine = engine(isolation, NetworkContext::new());
        let page = engine.create_page(PageParams::new(Viewport::new(200.0, 100.0)));
        let mut now = Instant::now();
        run_until(&mut engine, &mut now, |e| last_scene(e, page).is_some());
    }
}
