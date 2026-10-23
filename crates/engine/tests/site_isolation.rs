//! Milestone 41 (ADR 025), through the public API: a page's renderer holds
//! documents of one site. A cross-site navigation — a link, a typed URL, a
//! redirect — moves the page to another renderer; a same-site one does not;
//! the old document stays on screen until the new one commits. Pages of one
//! origin share `localStorage` in process mode, kept by the browser.
//!
//! Sites here: `127.0.0.1` and `localhost` are two sites (two hosts); two
//! ports of `127.0.0.1` are two origins of one site.

use engine::{
    Engine, EngineConfig, EngineEvent, EngineInput, ImageLoading, InProcessLauncher, Isolation, LoadRequest, NetworkContext, PageCommand, PageGrants, PageId,
    PageParams, Viewport,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

type Routes = Arc<Mutex<HashMap<String, (u16, String, String)>>>;
type Seen = Arc<Mutex<Vec<String>>>;

/// A server whose answers are set per path after it starts (pages link to
/// each other's ports). Every request path is recorded.
struct Server {
    port: u16,
    routes: Routes,
    seen: Seen,
}

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (routes, seen): (Routes, Seen) = Default::default();
        let (r, s) = (Arc::clone(&routes), Arc::clone(&seen));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (routes, seen) = (Arc::clone(&r), Arc::clone(&s));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path.clone());
                    let route = routes.lock().unwrap().get(path.split('?').next().unwrap_or("")).cloned();
                    let (status, extra, body) = route.unwrap_or((404, String::new(), "<html><body>none</body></html>".into()));
                    let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                });
            }
        });
        Server { port, routes, seen }
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }

    fn page(&self, path: &str, body: &str) {
        self.routes.lock().unwrap().insert(path.into(), (200, String::new(), format!("<html><body style=\"margin:0\">{body}</body></html>")));
    }

    fn redirect(&self, path: &str, to: &str) {
        self.routes.lock().unwrap().insert(path.into(), (302, format!("Location: {to}\r\n"), String::new()));
    }

    fn hits(&self, path: &str) -> usize {
        self.seen.lock().unwrap().iter().filter(|p| p.as_str() == path).count()
    }
}

/// A link filling the top of the page.
fn link_to(url: &str) -> String {
    format!("<a href=\"{url}\" style=\"display:block;height:100px\">go</a>")
}

fn engine() -> Engine {
    Engine::new(EngineConfig {
        network: NetworkContext::new(),
        waker: None,
        image_loading: ImageLoading::BeforeFirstFrame,
        isolation: Isolation::Processes(Arc::new(InProcessLauncher)),
        fonts: None,
    })
}

fn run_until(engine: &mut Engine, now: &mut Instant, all: &mut Vec<EngineEvent>, mut done: impl FnMut(&[EngineEvent]) -> bool) {
    let deadline = Instant::now() + WAIT;
    let start = all.len();
    loop {
        assert!(Instant::now() < deadline, "timed out; events so far: {:?}", &all[start..]);
        *now += Duration::from_millis(16);
        all.extend(engine.update_blocking(*now, WAIT));
        if done(&all[start..]) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn committed(url: &str) -> impl FnMut(&[EngineEvent]) -> bool + '_ {
    move |events| events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { url: Some(u), .. } if u == url))
}

fn failed(events: &[EngineEvent]) -> bool {
    events.iter().any(|e| matches!(e, EngineEvent::NavigationFailed { .. }))
}

fn click(engine: &mut Engine, page: PageId) {
    engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 20.0, y: 20.0 }));
    engine.send(page, PageCommand::Input(EngineInput::PointerUp { x: 20.0, y: 20.0 }));
}

/// No frame the page presented after `from` shows an empty page: a renderer
/// taking over holds back its blank first frame (ADR 025 §3).
fn no_blank_frames(events: &[EngineEvent]) {
    for e in events {
        if let EngineEvent::Frame { scene, .. } = e {
            assert!(!scene.items.is_empty(), "a blank frame was presented while the page moved");
        }
    }
}

/// The acceptance criterion: a cross-site link lands in a different
/// process; a same-site one does not.
#[test]
fn a_cross_site_link_lands_in_another_renderer_and_a_same_site_one_does_not() {
    let (a, b) = (Server::start(), Server::start());
    let (start, same_site, cross_site) = (a.url("127.0.0.1", "/start"), b.url("127.0.0.1", "/same"), a.url("localhost", "/other"));
    a.page("/start", &link_to(&same_site));
    b.page("/same", &link_to(&cross_site));
    a.page("/other", "<p>another site</p>");
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let (mut now, mut events) = (Instant::now(), Vec::new());
    engine.send(page, PageCommand::Load(LoadRequest::Url(start.clone())));
    run_until(&mut engine, &mut now, &mut events, committed(&start));
    let first = engine.renderer_serial(page).expect("a renderer");
    let shown = events.len();

    click(&mut engine, page);
    run_until(&mut engine, &mut now, &mut events, committed(&same_site));
    assert_eq!(engine.renderer_serial(page), Some(first), "another origin of the same site: the same renderer");

    click(&mut engine, page);
    run_until(&mut engine, &mut now, &mut events, committed(&cross_site));
    let second = engine.renderer_serial(page).unwrap();
    assert_ne!(second, first, "another site: another renderer");
    // The page's events read as one navigation: started once, committed once.
    let started = events.iter().filter(|e| matches!(e, EngineEvent::NavigationStarted { url, .. } if *url == cross_site)).count();
    assert_eq!(started, 1, "{events:?}");
    no_blank_frames(&events[shown..]);
    let commit = events.iter().rposition(|e| matches!(e, EngineEvent::NavigationCommitted { .. })).unwrap();
    assert!(events[commit..].iter().any(|e| matches!(e, EngineEvent::Frame { .. })), "the new renderer drew the new document");
    assert_eq!(a.hits("/other"), 1, "the browser fetched the new document once, for the new renderer");
}

/// A typed URL of another site, and a same-site link that redirects to
/// another site, move the page as well; a navigation that fails leaves it
/// where it was, its document still shown.
#[test]
fn a_typed_url_and_a_redirect_to_another_site_move_the_page_and_a_failure_does_not() {
    let a = Server::start();
    let (home, away, bounce, back) = (a.url("127.0.0.1", "/home"), a.url("localhost", "/away"), a.url("localhost", "/bounce"), a.url("127.0.0.1", "/back"));
    a.page("/home", "<p>home</p>");
    a.page("/away", &link_to(&bounce));
    a.redirect("/bounce", &back);
    a.page("/back", &link_to("http://localhost:1/unreachable"));
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let (mut now, mut events) = (Instant::now(), Vec::new());
    engine.send(page, PageCommand::Load(LoadRequest::Url(home.clone())));
    run_until(&mut engine, &mut now, &mut events, committed(&home));
    let first = engine.renderer_serial(page).unwrap();

    engine.send(page, PageCommand::Load(LoadRequest::Url(away.clone())));
    run_until(&mut engine, &mut now, &mut events, committed(&away));
    let second = engine.renderer_serial(page).unwrap();
    assert_ne!(second, first, "a typed URL of another site");

    // A link within `localhost` that lands on `127.0.0.1`: the document is
    // committed at the URL it landed on.
    click(&mut engine, page);
    run_until(&mut engine, &mut now, &mut events, committed(&back));
    let third = engine.renderer_serial(page).unwrap();
    assert_ne!(third, second, "the document is the redirect target's: its site's renderer");
    assert_eq!(a.hits("/bounce"), 1, "and it was not fetched twice");
    assert_eq!(a.hits("/back"), 1);

    let shown = events.len();
    click(&mut engine, page);
    run_until(&mut engine, &mut now, &mut events, failed);
    assert_eq!(engine.renderer_serial(page), Some(third), "a navigation that fails moves nothing");
    no_blank_frames(&events[shown..]);
}

/// A page without grants still moves between sites (the loader's requests
/// need none); and thread mode has no renderers to move between.
#[test]
fn thread_mode_has_no_renderer_to_move() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)));
    assert_eq!(engine.renderer_serial(page), None);
}

/// The acceptance criterion: two pages of one origin share `localStorage`
/// in process mode — the browser keeps it, in the profile's store — and a
/// page of another site does not see it.
#[test]
fn two_pages_of_one_origin_share_local_storage_in_process_mode() {
    let a = Server::start();
    a.page("/write", "<p>w</p><script>localStorage.setItem('greeting', 'from the first page')</script>");
    a.page("/read", "<p>r</p><script>fetch('/report?' + encodeURIComponent(localStorage.getItem('greeting')))</script>");
    let network = NetworkContext::new();
    let store = storage::StorageStore::new();
    let authority = capability::Authority::new();
    let grants = || PageGrants { network: Some(authority.network(network.clone())), storage: Some(authority.storage(store.clone())) };
    let mut engine = Engine::new(EngineConfig {
        network: network.clone(),
        waker: None,
        image_loading: ImageLoading::BeforeFirstFrame,
        isolation: Isolation::Processes(Arc::new(InProcessLauncher)),
        fonts: None,
    });
    let writer = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)).with_grants(grants()));
    let reader = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)).with_grants(grants()));
    let other = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)).with_grants(grants()));
    let (mut now, mut events) = (Instant::now(), Vec::new());
    let write = a.url("127.0.0.1", "/write");
    engine.send(writer, PageCommand::Load(LoadRequest::Url(write.clone())));
    run_until(&mut engine, &mut now, &mut events, committed(&write));
    assert_ne!(engine.renderer_serial(writer), engine.renderer_serial(reader), "two processes");
    let key = origin::StorageKey::first_party(&origin::Origin::parse(&write));
    assert_eq!(store.area_for(&key).get("greeting").as_deref(), Some("from the first page"), "kept by the browser, in the profile's store");

    let read = a.url("127.0.0.1", "/read");
    engine.send(reader, PageCommand::Load(LoadRequest::Url(read.clone())));
    let elsewhere = a.url("localhost", "/read");
    engine.send(other, PageCommand::Load(LoadRequest::Url(elsewhere.clone())));
    run_until(&mut engine, &mut now, &mut events, |_| a.seen.lock().unwrap().iter().filter(|p| p.starts_with("/report")).count() == 2);
    let reports: Vec<String> = a.seen.lock().unwrap().iter().filter(|p| p.starts_with("/report")).cloned().collect();
    assert!(reports.contains(&"/report?from%20the%20first%20page".to_string()), "the second page read the first one's write: {reports:?}");
    assert!(reports.contains(&"/report?null".to_string()), "a page of another site did not: {reports:?}");
}

/// A document is named by the URL it landed on, not the one it asked for
/// (M21's simplification, which M41 made matter): after a cross-site
/// redirect, its script's requests and storage are its landed-on origin's —
/// allowed by the renderer that holds that site — and its relative URLs
/// resolve against it. In thread mode too.
#[test]
fn after_a_cross_site_redirect_the_document_is_its_landed_on_origins() {
    for isolation in [Isolation::Threads, Isolation::Processes(Arc::new(InProcessLauncher))] {
        let a = Server::start();
        let (start, hop, landing) = (a.url("localhost", "/start"), a.url("localhost", "/hop"), a.url("127.0.0.1", "/landing"));
        a.page("/start", &link_to(&hop));
        a.redirect("/hop", &landing);
        // Relative: resolved against where it landed (`127.0.0.1`), the
        // report goes back to `/report` there.
        a.page("/landing", "<p>landed</p><script>localStorage.setItem('k', 'v'); fetch('report?' + localStorage.getItem('k'))</script>");
        let network = NetworkContext::new();
        let store = storage::StorageStore::new();
        let authority = capability::Authority::new();
        let grants = PageGrants { network: Some(authority.network(network.clone())), storage: Some(authority.storage(store.clone())) };
        let mut engine = Engine::new(EngineConfig { network, waker: None, image_loading: ImageLoading::BeforeFirstFrame, isolation, fonts: None });
        let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)).with_grants(grants));
        let (mut now, mut events) = (Instant::now(), Vec::new());
        engine.send(page, PageCommand::Load(LoadRequest::Url(start.clone())));
        run_until(&mut engine, &mut now, &mut events, committed(&start));
        click(&mut engine, page);
        run_until(&mut engine, &mut now, &mut events, committed(&landing));
        run_until(&mut engine, &mut now, &mut events, |_| a.hits("/report?v") == 1);
        let key = origin::StorageKey::first_party(&origin::Origin::parse(&landing));
        assert_eq!(store.area_for(&key).get("k").as_deref(), Some("v"), "stored under the landed-on origin");
        let asked = origin::StorageKey::first_party(&origin::Origin::parse(&hop));
        assert_eq!(store.area_for(&asked).get("k"), None, "not the origin it asked for");
    }
}
