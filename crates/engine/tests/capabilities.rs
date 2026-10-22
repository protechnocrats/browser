//! Milestone 36 (ADR 010), through the public API, in thread and process
//! mode: a page's scripts reach only what the host granted it, a revocation
//! sticks across navigations, and pages of different origins cannot observe
//! each other.

use engine::{
    CapabilityKind, Engine, EngineConfig, EngineEvent, ImageLoading, InProcessLauncher, Isolation, LoadRequest, NetworkContext, PageCommand, PageGrants, PageId,
    PageParams, Viewport,
};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn isolations() -> [(&'static str, Isolation); 2] {
    [("threads", Isolation::Threads), ("processes", Isolation::Processes(Arc::new(InProcessLauncher)))]
}

fn engine(isolation: Isolation, network: NetworkContext) -> Engine {
    Engine::new(EngineConfig { network, waker: None, image_loading: ImageLoading::BeforeFirstFrame, isolation, fonts: None })
}

/// Both capabilities, over `network` and a fresh store — what a profile
/// grants a web page.
fn granted(network: &NetworkContext) -> PageGrants {
    let authority = capability::Authority::new();
    PageGrants { network: Some(authority.network(network.clone())), storage: Some(authority.storage(storage::StorageStore::new())) }
}

/// A page at `/` whose script fetches `/api`; counts the `/api` requests.
fn serve() -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let api_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&api_hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let hits = Arc::clone(&hits);
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
                let body: &[u8] = if request.contains("/api") {
                    hits.fetch_add(1, Ordering::SeqCst);
                    b"data"
                } else if request.contains("/keys") {
                    b"<html><body><p>k</p><script>document.addEventListener('keydown', () => fetch('/api'))</script></body></html>"
                } else {
                    b"<html><body><p id=out>-</p><script>fetch('/api').then(r => r.text()).then(t => { document.getElementById('out').textContent = t }, e => { document.getElementById('out').textContent = String(e) })</script></body></html>"
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), api_hits)
}

/// Ticks the engine for `duration` (at least), or until `done`.
fn run_for(engine: &mut Engine, now: &mut Instant, duration: Duration, mut done: impl FnMut(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let deadline = Instant::now() + duration;
    let mut all = Vec::new();
    while Instant::now() < deadline {
        *now += Duration::from_millis(16);
        all.extend(engine.update_blocking(*now, WAIT));
        if done(&all) {
            break;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    all
}

fn committed(events: &[EngineEvent], page: PageId) -> bool {
    events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { page: p, .. } if *p == page))
}

#[test]
fn a_page_without_network_cannot_fetch_and_a_granted_one_can() {
    for (mode, isolation) in isolations() {
        let (base, api_hits) = serve();
        let network = NetworkContext::new();
        let mut engine = engine(isolation, network.clone());
        let before = diagnostics::metrics::snapshot();

        let denied = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
        let mut now = Instant::now();
        engine.send(denied, PageCommand::Load(LoadRequest::Url(format!("{base}/"))));
        let events = run_for(&mut engine, &mut now, WAIT, |e| committed(e, denied));
        assert!(committed(&events, denied), "{mode}: the engine still loads the document itself");
        run_for(&mut engine, &mut now, Duration::from_millis(400), |_| false);
        assert_eq!(api_hits.load(Ordering::SeqCst), 0, "{mode}: a page without Network never reaches the network from script");
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.capability_denied") >= 1, "{mode}: refused as a capability");

        let allowed = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)).with_grants(granted(&network)));
        engine.send(allowed, PageCommand::Load(LoadRequest::Url(format!("{base}/"))));
        run_for(&mut engine, &mut now, WAIT, |_| api_hits.load(Ordering::SeqCst) >= 1);
        assert_eq!(api_hits.load(Ordering::SeqCst), 1, "{mode}: the granted page's script fetched");
    }
}

#[test]
fn a_revoked_network_stays_revoked_for_later_documents() {
    for (mode, isolation) in isolations() {
        let (base, api_hits) = serve();
        let network = NetworkContext::new();
        let mut engine = engine(isolation, network.clone());
        let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)).with_grants(granted(&network)));
        assert_eq!(engine.page_grants(page), vec![CapabilityKind::Network, CapabilityKind::Storage]);
        let mut now = Instant::now();
        engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/"))));
        run_for(&mut engine, &mut now, WAIT, |_| api_hits.load(Ordering::SeqCst) >= 1);
        assert_eq!(api_hits.load(Ordering::SeqCst), 1, "{mode}");

        engine.send(page, PageCommand::Revoke(CapabilityKind::Network));
        assert_eq!(engine.page_grants(page), vec![CapabilityKind::Storage], "{mode}");
        // A new document of the same page: its script gets the revoked grant.
        engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/again"))));
        let events = run_for(&mut engine, &mut now, WAIT, |e| e.iter().filter(|e| matches!(e, EngineEvent::NavigationCommitted { .. })).count() >= 1);
        assert!(committed(&events, page), "{mode}: the document still loads");
        run_for(&mut engine, &mut now, Duration::from_millis(400), |_| false);
        assert_eq!(api_hits.load(Ordering::SeqCst), 1, "{mode}: no request after the revocation");
    }
}

/// The glyph count of `page`'s newest frame: a stand-in for "what it shows".
fn glyphs(events: &[EngineEvent], page: PageId) -> usize {
    let scene = events
        .iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Frame { page: p, scene, .. } if *p == page => Some(scene),
            _ => None,
        })
        .expect("a frame");
    engine::scene_to_json(scene).matches('[').count()
}

#[test]
fn pages_of_different_origins_cannot_observe_each_others_state() {
    for (mode, isolation) in isolations() {
        let network = NetworkContext::new();
        let mut engine = engine(isolation, network.clone());
        // One profile: every page's storage grant is over the same store.
        let authority = capability::Authority::new();
        let grants = PageGrants { network: Some(authority.network(network.clone())), storage: Some(authority.storage(storage::StorageStore::new())) };
        let new_page = |engine: &mut Engine| engine.create_page(PageParams::new(Viewport::new(600.0, 200.0)).with_grants(grants.clone()));
        let (writer, reader, fresh) = (new_page(&mut engine), new_page(&mut engine), new_page(&mut engine));
        let html = |url: &str, script: &str| {
            PageCommand::Load(LoadRequest::Html { html: format!("<html><body><p id=out>-</p><script>{script}</script></body></html>"), url: Some(url.into()) })
        };
        let secret = "'s3cret-'.repeat(8)";
        engine.send(writer, html("https://a.test/", &format!("localStorage.setItem('k', {secret}); sessionStorage.setItem('k', {secret}); globalThis.k = {secret};")));
        let mut now = Instant::now();
        run_for(&mut engine, &mut now, WAIT, |e| committed(e, writer));
        let show = "document.getElementById('out').textContent = [localStorage.getItem('k'), sessionStorage.getItem('k'), typeof globalThis.k].join('|');";
        engine.send(reader, html("https://b.test/", show));
        engine.send(fresh, html("https://c.test/", show));
        let events = run_for(&mut engine, &mut now, WAIT, |e| committed(e, reader) && committed(e, fresh));
        assert_eq!(glyphs(&events, reader), glyphs(&events, fresh), "{mode}: b.test sees exactly what an origin that never wrote sees");
    }
}

/// M36's gap: a page whose network grant was revoked can be granted it
/// again — by the document showing now, at once, as by later ones — with a
/// fresh handle, so the revoked one stays revoked.
#[test]
fn a_revoked_grant_can_be_granted_again_to_the_live_page() {
    for (mode, isolation) in isolations() {
        let (base, api_hits) = serve();
        let network = NetworkContext::new();
        let mut engine = engine(isolation, network.clone());
        let first = granted(&network);
        let old_network = first.network.clone().unwrap();
        let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)).with_grants(first));
        engine.send(page, PageCommand::Revoke(CapabilityKind::Network));
        let mut now = Instant::now();
        engine.send(page, PageCommand::Load(LoadRequest::Url(format!("{base}/keys"))));
        let events = run_for(&mut engine, &mut now, WAIT, |e| committed(e, page));
        assert!(committed(&events, page), "{mode}");
        let press = |engine: &mut Engine| engine.send(page, PageCommand::Input(engine::EngineInput::Key { key: "a".into() }));

        press(&mut engine);
        run_for(&mut engine, &mut now, Duration::from_millis(400), |_| false);
        assert_eq!(api_hits.load(Ordering::SeqCst), 0, "{mode}: revoked");

        let again = capability::Authority::new().network(network.clone());
        engine.send(page, PageCommand::Grant(PageGrants { network: Some(again), storage: None }));
        assert_eq!(engine.page_grants(page), vec![CapabilityKind::Network, CapabilityKind::Storage], "{mode}");
        press(&mut engine);
        run_for(&mut engine, &mut now, WAIT, |_| api_hits.load(Ordering::SeqCst) >= 1);
        assert_eq!(api_hits.load(Ordering::SeqCst), 1, "{mode}: the live document's script has the network again");
        assert!(old_network.is_revoked(), "{mode}: the revoked handle stays revoked");
    }
}
