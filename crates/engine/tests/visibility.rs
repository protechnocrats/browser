//! Milestone 38b (ADR 020), through the public API: a hidden page does no
//! animation work and builds no frames, but still settles its fetches; shown
//! again, it draws at once and animates again. On a thread and in a renderer
//! process.

use engine::{Engine, EngineConfig, EngineEvent, ImageLoading, InProcessLauncher, Isolation, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use rendering::DisplayItem;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A page that animates three ways: a rAF loop, a CSS animation and a
/// transition-free colour flip from script on every frame.
const ANIMATED: &str = "<html><head><style>\
    @keyframes spin { from { opacity: 0.2 } to { opacity: 1 } }\
    #a { width: 40px; height: 40px; background-color: #ff0000; animation: spin 1s infinite }\
    </style></head><body><div id=a></div><p id=n>0</p><script>\
    var n = 0; function f() { n++; document.getElementById('n').textContent = n; requestAnimationFrame(f); }\
    requestAnimationFrame(f);</script></body></html>";

struct Host {
    engine: Engine,
    woken: Arc<AtomicUsize>,
}

impl Host {
    fn new(isolation: Isolation, network: engine::NetworkContext) -> Host {
        let mut engine = Engine::new(EngineConfig { network, image_loading: ImageLoading::Progressive, isolation, ..EngineConfig::default() });
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        engine.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        Host { engine, woken }
    }

    /// Runs as a desktop host does — updating only when woken or due — for
    /// `duration`. Returns the events and how many updates it made.
    fn run(&mut self, duration: Duration) -> (Vec<EngineEvent>, usize) {
        let end = Instant::now() + duration;
        let (mut events, mut updates) = (Vec::new(), 0);
        while Instant::now() < end {
            let due = self.engine.next_wake().is_some_and(|at| at <= Instant::now());
            if self.woken.swap(0, Ordering::SeqCst) == 0 && !due {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            updates += 1;
            events.extend(self.engine.update(Instant::now()));
        }
        (events, updates)
    }
}

fn frames(events: &[EngineEvent], page: PageId) -> Vec<&rendering::Scene> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Frame { page: p, scene, .. } if *p == page => Some(scene),
            _ => None,
        })
        .collect()
}

fn html(page: &str) -> PageCommand {
    PageCommand::Load(LoadRequest::Html { html: page.into(), url: Some("http://127.0.0.1:9/".into()) })
}

fn a_hidden_page_does_no_animation_work(isolation: Isolation) {
    let mut host = Host::new(isolation, engine::NetworkContext::new());
    let page = host.engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    host.engine.send(page, html(ANIMATED));
    let (events, _) = host.run(Duration::from_millis(400));
    assert!(frames(&events, page).len() >= 10, "visible, it animates: {} frames", frames(&events, page).len());

    host.engine.send(page, PageCommand::SetVisible(false));
    host.run(Duration::from_millis(50)); // the command lands
    let (events, updates) = host.run(Duration::from_millis(600));
    assert!(frames(&events, page).is_empty(), "hidden, it drew {} frames", frames(&events, page).len());
    assert!(updates <= 2, "hidden, the host was asked for {updates} updates");
    assert!(host.engine.next_wake().is_none(), "a hidden page asks for no wake-up");

    host.engine.send(page, PageCommand::SetVisible(true));
    let started = Instant::now();
    let (events, _) = host.run(Duration::from_millis(400));
    let shown = frames(&events, page);
    assert!(shown.len() >= 10, "shown again, it animates: {} frames", shown.len());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_hidden_page_does_no_animation_work_on_a_thread() {
    a_hidden_page_does_no_animation_work(Isolation::Threads);
}

#[test]
fn a_hidden_page_does_no_animation_work_in_a_renderer_process() {
    a_hidden_page_does_no_animation_work(Isolation::Processes(Arc::new(InProcessLauncher)));
}

const FETCHING: &str = "<html><body><div id=b style=\"width: 40px; height: 40px; background-color: #000000\"></div><script>\
    fetch('/data').then(function (r) { return r.text(); }).then(function (t) {\
    if (t === 'ok') document.getElementById('b').style.backgroundColor = '#00ff00'; });</script></body></html>";

/// Serves [`FETCHING`] at `/` at once, and "ok" everywhere else after `delay`.
fn serve(delay: Duration) -> String {
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
                let body = if request.split_whitespace().nth(1) == Some("/") {
                    FETCHING
                } else {
                    std::thread::sleep(delay);
                    "ok"
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

fn a_hidden_page_still_settles_its_fetches(isolation: Isolation) {
    let network = engine::NetworkContext::new();
    let mut host = Host::new(isolation, network.clone());
    let grants = capability::PageGrants { network: Some(capability::Authority::new().network(network)), storage: None };
    let page = host.engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)).with_grants(grants));
    // Loaded by URL: in process mode the browser lets a page's script
    // request only origins it navigated to (ADR 016 §5).
    host.engine.send(page, PageCommand::Load(LoadRequest::Url(serve(Duration::from_millis(300)))));
    host.engine.send(page, PageCommand::SetVisible(false));
    let (events, _) = host.run(Duration::from_millis(900));
    assert!(frames(&events, page).is_empty(), "hidden from the start: no frames");

    host.engine.send(page, PageCommand::SetVisible(true));
    let (events, _) = host.run(Duration::from_millis(300));
    let shown = frames(&events, page);
    let green = shown.last().is_some_and(|s| {
        s.items.iter().any(|i| matches!(i, DisplayItem::Rect { color, .. } if (color.r, color.g, color.b) == (0, 255, 0)))
    });
    assert!(green, "the fetch settled and its script ran while the page was hidden; the first frame shows it");
}

#[test]
fn a_hidden_page_still_settles_its_fetches_on_a_thread() {
    a_hidden_page_still_settles_its_fetches(Isolation::Threads);
}

#[test]
fn a_hidden_page_still_settles_its_fetches_in_a_renderer_process() {
    a_hidden_page_still_settles_its_fetches(Isolation::Processes(Arc::new(InProcessLauncher)));
}

/// A page whose box `#t` starts a 3 s `background-color` transition two
/// frames after it loads, `top_px` from the top of the page.
fn transition_page(top_px: u32) -> String {
    format!(
        "<html><head><style>#t {{ margin-top: {top_px}px; width: 40px; height: 40px; background-color: #ff0000; transition: background-color 3s linear }}</style></head>\
         <body><div id=t></div><script>\
         requestAnimationFrame(function () {{ requestAnimationFrame(function () {{ document.getElementById('t').style.backgroundColor = '#0000ff'; }}); }});\
         </script></body></html>"
    )
}

/// M38b's gap (ADR 020, the first degrade step): a transition nobody can
/// see — `background-color` on a box below the viewport — builds no frames
/// and lets the page sleep; the same transition on screen animates; and
/// scrolled into view, the box shows the colour the time says.
fn an_off_screen_transition_builds_no_frames(isolation: Isolation) {
    let mut host = Host::new(isolation, engine::NetworkContext::new());
    let below = host.engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    host.engine.send(below, html(&transition_page(3000)));
    let on_screen = host.engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    host.engine.send(on_screen, html(&transition_page(20)));
    host.run(Duration::from_millis(300)); // loaded, both transitions started
    let (events, _) = host.run(Duration::from_millis(600));
    assert!(frames(&events, on_screen).len() >= 10, "on screen it animates: {} frames", frames(&events, on_screen).len());
    assert!(frames(&events, below).is_empty(), "off screen it built {} frames", frames(&events, below).len());

    host.engine.send(below, PageCommand::Input(engine::EngineInput::Scroll { dx: 0.0, dy: 2950.0 }));
    let (events, _) = host.run(Duration::from_millis(300));
    let shown = frames(&events, below);
    assert!(shown.len() >= 5, "scrolled into view, it animates: {} frames", shown.len());
    let colour = shown.last().unwrap().items.iter().find_map(|i| match i {
        DisplayItem::Rect { color, width, .. } if *width == 40.0 => Some(*color),
        _ => None,
    });
    let colour = colour.expect("the box is drawn");
    assert!(colour.b > 0 && colour.r < 255, "mid-transition, by the clock: {colour:?}");
}

#[test]
fn an_off_screen_transition_builds_no_frames_on_a_thread() {
    an_off_screen_transition_builds_no_frames(Isolation::Threads);
}

#[test]
fn an_off_screen_transition_builds_no_frames_in_a_renderer_process() {
    an_off_screen_transition_builds_no_frames(Isolation::Processes(Arc::new(InProcessLauncher)));
}
