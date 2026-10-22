//! Milestone 37 through the public API, in thread and process mode: a page
//! under the private fingerprint policy sees UTC and coarse time; the filter
//! blocks a page's subresources in the browser process before they leave it
//! (ADR 019).

use engine::{
    Engine, EngineConfig, EngineEvent, FingerprintPolicy, ImageLoading, InProcessLauncher, Isolation, LoadRequest, NetworkContext, PageCommand, PageId,
    PageParams, Viewport,
};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn isolations() -> [(&'static str, Isolation); 2] {
    [("threads", Isolation::Threads), ("processes", Isolation::Processes(Arc::new(InProcessLauncher)))]
}

fn engine(isolation: Isolation, network: NetworkContext) -> Engine {
    Engine::new(EngineConfig { network, waker: None, image_loading: ImageLoading::BeforeFirstFrame, isolation, fonts: None })
}

fn run_until(engine: &mut Engine, now: &mut Instant, mut done: impl FnMut(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let deadline = Instant::now() + WAIT;
    let mut all = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "timed out: {all:?}");
        *now += Duration::from_millis(16);
        all.extend(engine.update_blocking(*now, WAIT));
        if done(&all) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn committed(events: &[EngineEvent], page: PageId) -> bool {
    events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { page: p, .. } if *p == page))
}

fn glyphs(events: &[EngineEvent], page: PageId) -> usize {
    let scene = events.iter().rev().find_map(|e| match e {
        EngineEvent::Frame { page: p, scene, .. } if *p == page => Some(scene),
        _ => None,
    });
    engine::scene_to_json(scene.expect("a frame")).matches('[').count()
}

#[test]
fn a_private_page_sees_utc_and_coarse_time_in_both_modes() {
    for (mode, isolation) in isolations() {
        let mut engine = engine(isolation, NetworkContext::new());
        let private = engine.create_page(PageParams::new(Viewport::new(400.0, 100.0)).with_fingerprint(FingerprintPolicy::private()));
        let reference = engine.create_page(PageParams::new(Viewport::new(400.0, 100.0)));
        let page = |body: &str| PageCommand::Load(LoadRequest::Html { html: format!("<html><body><p id=o>-</p><script>{body}</script></body></html>"), url: None });
        // What a private page should show, computed by script...
        engine.send(private, page("var ok = true; for (var i = 0; i < 100; i++) { if (Date.now() % 100) ok = false; } document.getElementById('o').textContent = [ok, new Date(0).getTimezoneOffset(), new Date(Date.UTC(2026, 0, 1, 7)).getHours()].join(',');"));
        // ...and the same text written out.
        engine.send(reference, page("document.getElementById('o').textContent = 'true,0,7';"));
        let mut now = Instant::now();
        let events = run_until(&mut engine, &mut now, |e| committed(e, private) && committed(e, reference));
        assert_eq!(glyphs(&events, private), glyphs(&events, reference), "{mode}: the private page computed 'true,0,7'");
    }
}

/// Serves a page whose image URL the filter blocks; counts image requests.
fn serve() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let pixel_hits = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (hits, seen) = (Arc::clone(&pixel_hits), Arc::clone(&log));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (hits, seen) = (Arc::clone(&hits), Arc::clone(&seen));
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
                seen.lock().unwrap().push(request.trim().to_string());
                let body: &[u8] = if request.contains("/pixel/") {
                    hits.fetch_add(1, Ordering::SeqCst);
                    b"GIF89a"
                } else {
                    b"<html><body><p>article</p><img src=\"/pixel/track.gif\" width=1 height=1></body></html>"
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), pixel_hits, log)
}

#[test]
fn the_filter_blocks_a_pages_subresource_in_the_browser_process() {
    for (mode, isolation) in isolations() {
        let (base, pixel_hits, log) = serve();
        let network = NetworkContext::new();
        let mut filter = network::FilterEngine::new();
        filter.add_list(network::FilterCategory::Trackers, "/pixel/*$image").unwrap();
        network.set_filter(filter);
        let before = diagnostics::metrics::snapshot();
        let mut engine = engine(isolation, network);
        let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
        engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/article"))));
        let mut now = Instant::now();
        let events = run_until(&mut engine, &mut now, |e| committed(e, page));
        assert!(committed(&events, page), "{mode}: the page itself loads");
        assert_eq!(pixel_hits.load(Ordering::SeqCst), 0, "{mode}: the tracking pixel never left: {:?}", log.lock().unwrap());
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.request_blocked") >= 1, "{mode}");
    }
}
