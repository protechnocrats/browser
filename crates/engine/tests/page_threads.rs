//! Milestone 30 acceptance: pages on their own threads (ADR 006), crash
//! containment (ADR 007), backpressure, validation on receipt. Public API
//! only, like `headless_host.rs`.

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn html(html: &str) -> PageCommand {
    PageCommand::Load(LoadRequest::Html { html: html.to_string(), url: None })
}

fn frame_for(events: &[EngineEvent], page: PageId) -> Option<&engine::Scene> {
    events.iter().find_map(|e| match e {
        EngineEvent::Frame { page: p, scene, .. } if *p == page => Some(scene),
        _ => None,
    })
}

fn new_page(engine: &mut Engine) -> PageId {
    engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)))
}

/// A page whose click handler keeps its thread busy for 1.5 s of wall-clock
/// time (well under the script loop limit, well over a frame) — not a fixed
/// amount of work, which took 19 s in a debug build (past `WAIT`).
const BUSY_ON_CLICK: &str = "<html><head><style>#b { width: 400px; height: 300px; }</style></head><body>\
    <div id=\"b\">busy</div><script>\
    document.getElementById('b').addEventListener('click', function () { \
      var x = 0; var end = Date.now() + 1500; while (Date.now() < end) { x++; } \
      document.getElementById('b').textContent = 'done ' + (x > 0); });\
    </script></body></html>";

#[test]
fn a_page_panic_is_contained_to_that_page() {
    let mut engine = Engine::new(EngineConfig::default());
    let (a, b) = (new_page(&mut engine), new_page(&mut engine));
    engine.send(a, html("<html><body><p>a</p></body></html>"));
    engine.send(b, html("<html><body><p>b</p></body></html>"));
    let now = Instant::now();
    engine.update_blocking(now, WAIT);

    engine.send(a, PageCommand::SimulateCrash);
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    let crash = events.iter().find_map(|e| match e {
        EngineEvent::PageCrashed { page, reason } => Some((*page, reason.clone())),
        _ => None,
    });
    let (crashed, reason) = crash.expect("the crash is reported as an event");
    assert_eq!(crashed, a);
    assert!(reason.contains("simulated crash"), "{reason}");

    // The host (this thread) is fine, and page B still works.
    engine.send(b, PageCommand::Resize(Viewport::new(500.0, 300.0)));
    let events = engine.update_blocking(now + Duration::from_millis(32), WAIT);
    assert_eq!(frame_for(&events, b).map(|s| s.viewport_width), Some(500.0), "the other page is unaffected");
    // The crashed page stays crashed: no more events, commands dropped, reported once.
    engine.send(a, html("<html><body><p>again</p></body></html>"));
    let later = engine.update_blocking(now + Duration::from_millis(48), WAIT);
    assert!(!later.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })), "a crash is reported once");
    assert!(frame_for(&later, a).is_none(), "a crashed page draws nothing more");
}

#[test]
fn thousands_of_rapid_inputs_cannot_grow_a_pages_inbox_without_bound() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = new_page(&mut engine);
    // No update in between: nothing is consumed while these arrive.
    for i in 0..1000 {
        engine.send(page, PageCommand::Input(EngineInput::PointerMove { x: i as f32, y: 1.0 }));
        engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 1.0 }));
    }
    assert_eq!(engine.queued_commands(page), 2, "moves and scrolls coalesce");
    for i in 0..5000 {
        engine.send(page, PageCommand::Input(EngineInput::Key { key: format!("{}", i % 10) }));
    }
    assert!(engine.queued_commands(page) <= engine::MAX_QUEUED_COMMANDS, "{}", engine.queued_commands(page));
    // And the page still processes what it kept.
    let events = engine.update_blocking(Instant::now(), WAIT);
    assert!(frame_for(&events, page).is_some());
    assert_eq!(engine.queued_commands(page), 0);
}

#[test]
fn hostile_messages_are_rejected_not_trusted() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = new_page(&mut engine);
    engine.send(page, html("<html><body><p>ok</p></body></html>"));
    let now = Instant::now();
    engine.update_blocking(now, WAIT);

    for command in [
        PageCommand::Input(EngineInput::PointerDown { x: f32::NAN, y: f32::NAN }),
        PageCommand::Input(EngineInput::Scroll { dx: f32::INFINITY, dy: 0.0 }),
        PageCommand::Input(EngineInput::Key { key: "k".repeat(10 * 1024 * 1024) }),
    ] {
        engine.send(page, command);
    }
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    assert!(frame_for(&events, page).is_none(), "nothing valid arrived, so nothing changed");

    // A 4 GiB viewport is clamped, not honoured.
    engine.send(page, PageCommand::Resize(Viewport { width: 4.0 * 1024.0 * 1024.0 * 1024.0, height: 300.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(32), WAIT);
    assert_eq!(frame_for(&events, page).map(|s| s.viewport_width), Some(engine::MAX_VIEWPORT_PX));

    // An oversized document is refused with a failure the host can see.
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: "x".repeat(engine::MAX_HTML_BYTES + 1), url: None }));
    let events = engine.update_blocking(now + Duration::from_millis(48), WAIT);
    assert!(events.iter().any(|e| matches!(e, EngineEvent::NavigationFailed { error, .. } if error.contains("too large"))), "{events:?}");
}

#[test]
fn two_pages_render_concurrently_and_a_busy_page_blocks_neither_the_host_nor_the_other_page() {
    let mut engine = Engine::new(EngineConfig::default());
    let (busy, other) = (new_page(&mut engine), new_page(&mut engine));
    engine.send(busy, html(BUSY_ON_CLICK));
    engine.send(other, html("<html><body><p id=\"p\">other</p><script>localStorage.setItem('k', 'other');</script></body></html>"));
    let start = Instant::now();
    engine.update_blocking(start, WAIT);

    // Keep `busy`'s thread in a long click handler...
    engine.send(busy, PageCommand::Input(EngineInput::PointerDown { x: 10.0, y: 10.0 }));
    // ...while `other` has work too.
    engine.send(other, PageCommand::Resize(Viewport::new(333.0, 300.0)));
    let posted = Instant::now();
    let mut events = engine.update(start + Duration::from_millis(16));
    assert!(posted.elapsed() < Duration::from_millis(50), "update() posts ticks; it never waits for a page ({:?})", posted.elapsed());

    let deadline = Instant::now() + WAIT;
    while frame_for(&events, other).is_none() {
        assert!(Instant::now() < deadline, "the other page never rendered");
        std::thread::sleep(Duration::from_millis(1));
        events.extend(engine.update(start + Duration::from_millis(16)));
    }
    assert_eq!(frame_for(&events, other).unwrap().viewport_width, 333.0);
    assert!(frame_for(&events, busy).is_none(), "the busy page had not finished when the other page's frame arrived");

    // The busy page does finish, on its own thread.
    let events = engine.update_blocking(start + Duration::from_millis(32), WAIT);
    assert!(frame_for(&events, busy).is_some(), "the busy page finished its click");
}

#[test]
fn a_page_stuck_in_a_script_can_still_be_closed_without_waiting() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = new_page(&mut engine);
    engine.send(page, html(BUSY_ON_CLICK));
    let now = Instant::now();
    engine.update_blocking(now, WAIT);
    engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 5.0, y: 5.0 }));
    engine.update(now + Duration::from_millis(16));
    let closing = Instant::now();
    engine.close_page(page);
    drop(engine);
    assert!(closing.elapsed() < Duration::from_millis(50), "closing does not join a busy page ({:?})", closing.elapsed());
}

#[test]
fn the_waker_fires_when_a_page_publishes() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let mut engine = Engine::new(EngineConfig { waker: Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })), ..EngineConfig::default() });
    let page = new_page(&mut engine);
    engine.send(page, html("<html><body><p>hi</p></body></html>"));
    engine.update_blocking(Instant::now(), WAIT);
    assert!(wakes.load(Ordering::SeqCst) >= 1);
    assert_eq!(engine.next_wake(), None, "with a waker installed, an idle engine needs no polling");
}

#[test]
fn pages_of_one_origin_share_local_storage_across_threads_and_other_origins_do_not() {
    let mut engine = Engine::new(EngineConfig::default());
    // One profile store, granted to all three pages (ADR 010): sharing is
    // decided by each document's storage key, not by which page it is in.
    let authority = capability::Authority::new();
    let grants = engine::PageGrants { network: None, storage: Some(authority.storage(storage::StorageStore::new())) };
    let mut granted_page = || engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)).with_grants(grants.clone()));
    let (a1, a2, b) = (granted_page(), granted_page(), granted_page());
    let load = |url: &str, script: &str| PageCommand::Load(LoadRequest::Html {
        html: format!("<html><body><p id=\"out\">-</p><script>{script}</script></body></html>"),
        url: Some(url.to_string()),
    });
    engine.send(a1, load("https://a.test/1", "localStorage.setItem('user', 'alice');"));
    let now = Instant::now();
    engine.update_blocking(now, WAIT);
    let show = "document.getElementById('out').textContent = String(localStorage.getItem('user'));";
    engine.send(a2, load("https://a.test/2", show));
    engine.send(b, load("https://b.test/", show));
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    let glyphs = |p| engine::scene_to_json(frame_for(&events, p).unwrap()).matches('[').count();
    // "alice" draws more glyphs than "null".
    assert!(glyphs(a2) > glyphs(b), "same origin sees the other thread's write; a different origin sees null");
}
